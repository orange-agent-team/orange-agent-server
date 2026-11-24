#!/usr/bin/env python3.12
"""Run one local development attempt on an explicit NVIDIA GPU; keep uncertain attempts fenced."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time

from linux_platform_inputs import local_image_id

PROFILE="sdxl-base-1.0-euler-fp16-cuda-v1"
MAX_PNG=16*1024*1024


def require(ok,reason):
    if not ok:raise RuntimeError(reason)


def local_dir(path):
    require(path.is_absolute() and not path.is_symlink() and path.is_dir() and path.stat().st_uid==os.getuid()
            and path.stat().st_mode&0o077==0 and not any(c in str(path) for c in (",","\n","\r")),"Expected private operator directory")
    return path.resolve(strict=True)


def gpu_usage(device):
    smi=next((p for p in (Path("/usr/bin/nvidia-smi"),Path("/usr/lib/wsl/lib/nvidia-smi")) if p.is_file()),None)
    require(smi is not None,"NVIDIA driver observation unavailable")
    def query(fields):
        return subprocess.check_output([smi,"-i",device,fields,"--format=csv,noheader,nounits"],text=True,timeout=8,
                                       env={"PATH":"/usr/bin:/bin","LC_ALL":"C"}).strip()
    cols=[v.strip() for v in query("--query-gpu=uuid,memory.used").split(",")]
    require(len(cols)==2 and cols[0].lower()==device.lower() and re.fullmatch(r"[0-9]+",cols[1]),"Incomplete GPU memory observation")
    require(not query("--query-compute-apps=pid"),"Device still has compute clients")
    return int(cols[1])


def recovered_gpu(device,baseline,observe=gpu_usage,interval=.5,timeout=15):
    deadline=time.monotonic()+timeout;consecutive=0
    while time.monotonic()<deadline:
        try:used=observe(device)
        except (OSError,subprocess.SubprocessError,RuntimeError):consecutive=0
        else:
            consecutive=consecutive+1 if used<=baseline else 0
            if consecutive>=2:return True
        time.sleep(interval)
    return False


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--qualification",required=True,type=Path,help="Verified local OCI qualification receipt")
    cli.add_argument("--qualification-sha256",required=True,help="Digest from the operator-approved release registry")
    cli.add_argument("--gpu-uuid",required=True)
    cli.add_argument("--weights",required=True,type=Path)
    cli.add_argument("--work-root",required=True,type=Path)
    cli.add_argument("--output",required=True,type=Path,help="New PNG in a private owner-controlled directory")
    cli.add_argument("--context",default="default")
    args=cli.parse_args()
    require(sys.platform=="linux","Target launcher requires local Linux or WSL with driver visibility")
    require(os.getuid()!=0,"Run as the non-root owner of the development workspace")
    require(re.fullmatch(r"GPU-[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}",args.gpu_uuid),"One NVIDIA GPU UUID required")
    require(args.output.is_absolute() and args.output.suffix==".png" and not os.path.lexists(args.output),"Expected a new output PNG")
    output=local_dir(args.output.parent)/args.output.name
    weights=local_dir(args.weights);work=local_dir(args.work_root)
    require(not work.is_relative_to(weights) and not weights.is_relative_to(work) and not output.is_relative_to(weights),"Separate model cache and attempt roots required")
    raw_proof=args.qualification.read_bytes()
    require(re.fullmatch("[0-9a-f]{64}",args.qualification_sha256) and hashlib.sha256(raw_proof).hexdigest()==args.qualification_sha256,"Qualification differs from approved registry")
    proof=json.loads(raw_proof);engine=["docker","--context",args.context]
    endpoints=json.loads(subprocess.check_output(["docker","context","inspect",args.context],timeout=10))[0]["Endpoints"]
    require(endpoints["docker"]["Host"].startswith("unix://"),"Local Docker daemon with the selected device and private mounts required")
    require(proof["platform"]=="linux/amd64" and proof["published"] is False and proof["no_device_denied"] is True,"Unqualified runtime")
    image=local_image_id(engine,{"image":"orange-dev-input/sdxl-cuda@"+proof["image"],"config_digest":proof["config_digest"],"platform":"linux/amd64"})
    require(image,"Target image must already be imported and verified")
    # Validate with the exact fixed frame parser. No caller supplies command line, mounts, capabilities or network destinations.
    from importlib.util import spec_from_file_location,module_from_spec
    module=spec_from_file_location("orange_sdxl_frame",Path(__file__).resolve().parents[1]/"distribution/sdxl-linux/runner.py")
    parser=module_from_spec(module);module.loader.exec_module(parser)
    raw=sys.stdin.buffer.read(8193);request=parser.frame(raw)
    slot=work/("gpu-"+args.gpu_uuid.lower())
    slot.mkdir(mode=0o700,exist_ok=True);local_dir(slot)
    import fcntl
    lock_fd=os.open(slot/"attempt.lock",os.O_CREAT|os.O_RDWR|os.O_NOFOLLOW,0o600)
    with os.fdopen(lock_fd,"a") as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        journal=slot/"attempt.json"
        require(not os.path.lexists(journal),"Prior device attempt needs reconciliation")
        baseline=gpu_usage(args.gpu_uuid)
        attempt=Path(tempfile.mkdtemp(prefix="attempt-",dir=slot))
        scratch=attempt/"scratch";result=attempt/"result"
        scratch.mkdir(mode=0o700);result.mkdir(mode=0o700)
        name="orange-sdxl-"+args.gpu_uuid.lower();nonce=request["attempt_id"]
        def save(phase,cid=None):
            tmp=attempt/"journal.tmp"
            with tmp.open("w") as out:
                json.dump({"phase":phase,"attempt":nonce,"image":proof["image"],"device":args.gpu_uuid,"container":cid,"private_directory":attempt.name,"baseline_used_mib":baseline,"directory_device":attempt.stat().st_dev,"directory_inode":attempt.stat().st_ino},out)
                out.flush();os.fsync(out.fileno())
            os.replace(tmp,journal)
            fd=os.open(slot,os.O_DIRECTORY|os.O_RDONLY)
            try:os.fsync(fd)
            finally:os.close(fd)
        save("creating")
        cid=None;reaped=False;gpu_released=False;receipt=None
        def checked_state():
            row=json.loads(subprocess.check_output([*engine,"inspect",cid or name],timeout=15))[0]
            require(row["Config"]["Labels"].get("orange.owned")==nonce and row["Name"].lstrip("/")==name,"Different container owns device slot")
            return row["State"]
        def stop(signum,frame):raise InterruptedError("Attempt interrupted")
        old={s:signal.signal(s,stop) for s in (signal.SIGINT,signal.SIGTERM)}
        try:
            cmd=[*engine,"create","-i","--pull=never","--platform","linux/amd64","--name",name,"--label","orange.owned="+nonce,
                 "--network","none","--read-only","--cap-drop","ALL","--security-opt","no-new-privileges","--pids-limit","128",
                 "--memory","24g","--cpus","2","--user",f"{os.getuid()}:{os.getgid()}","--gpus","device="+args.gpu_uuid,
                 "--tmpfs","/tmp:rw,nosuid,nodev,noexec,mode=1777,size=512m",
                 "--mount",f"type=bind,source={weights},target=/weights,readonly",
                 "--mount",f"type=bind,source={scratch},target=/scratch","--mount",f"type=bind,source={result},target=/result",image,"generate"]
            cid=subprocess.check_output(cmd,text=True,timeout=45).strip()
            require(re.fullmatch("[0-9a-f]{64}",cid),"Invalid owned container")
            checked_state();save("starting",cid)
            with tempfile.TemporaryFile(dir=attempt) as stream,subprocess.Popen([*engine,"start","-a","-i",cid],stdin=subprocess.PIPE,stdout=stream,stderr=subprocess.DEVNULL) as child:
                try:
                    child.stdin.write(raw);child.stdin.close()
                    deadline=time.monotonic()+1815
                    while child.poll() is None:
                        require(time.monotonic()<deadline and os.fstat(stream.fileno()).st_size<=65536,"Deadline or frame limit")
                        time.sleep(.2)
                    state=checked_state();require(not state["Running"] and state["Status"]=="exited","Missing exit confirmation")
                    reaped=True;save("exited",cid)
                    require(child.returncode==0 and state["ExitCode"]==0,"No successful image completion")
                    stream.seek(0);wire=stream.read(65537);require(len(wire)<=65536,"Frame limit")
                    frames=[json.loads(line,object_pairs_hook=parser.unique) for line in wire.splitlines()]
                finally:
                    if child.poll() is None:child.kill();child.wait(timeout=10)
            require(len(frames)==26 and all(f=={"phase":"denoising","attempt_id":nonce,"completed_steps":i+1} for i,f in enumerate(frames[:-1])),"Invalid step sequence")
            done=frames[-1];candidate=result/"candidate.png"
            require(done["phase"]=="complete" and done["model_version"]==PROFILE and done["attempt_id"]==nonce and done["seed"]==request["seed"] and
                    (done["width"],done["height"],done["steps"],done["media_type"])==(1024,1024,25,"image/png"),"Wrong result identity")
            fd=os.open(candidate,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
            with os.fdopen(fd,"rb") as inp:
                m=os.fstat(fd);require(stat.S_ISREG(m.st_mode) and m.st_nlink==1 and 0<m.st_size==done["bytes"]<=MAX_PNG,"Invalid result file")
                data=inp.read(MAX_PNG+1)
            require(len(data)==done["bytes"] and hashlib.sha256(data).hexdigest()==done["sha256"],"Changed PNG")
            # The host independently decodes the exact bytes before publishing them.
            import io
            from PIL import Image
            with Image.open(io.BytesIO(data)) as decoded:
                require(decoded.format=="PNG" and decoded.size==(1024,1024) and decoded.mode=="RGB","Invalid image structure")
                require(hashlib.sha256(decoded.tobytes()).hexdigest()==done["rgb_sha256"],"Decoded pixels differ")
            gpu_released=recovered_gpu(args.gpu_uuid,baseline)
            require(gpu_released,"Device memory/compute quiescence not confirmed")
            save("release_confirmed",cid)
            with tempfile.NamedTemporaryFile(dir=output.parent,delete=False) as out:
                temp=Path(out.name)
                try:
                    out.write(data);out.flush();os.fsync(out.fileno());os.link(temp,output)
                finally:temp.unlink(missing_ok=True)
            receipt={**done,"device_uuid":args.gpu_uuid,"baseline_used_mib":baseline,"gpu_release_observed":True}
        finally:
            for sig,handler in old.items():signal.signal(sig,handler)
            if cid is None:
                # Distinguish refusal before creation from a lost response after creation.
                listed=subprocess.check_output([*engine,"ps","-a","--no-trunc","--filter","name=^/"+name+"$","--format","{{.ID}}"],text=True,timeout=15).splitlines()
                if not listed:
                    journal.unlink();shutil.rmtree(attempt);reaped=True
                elif len(listed)==1 and re.fullmatch("[0-9a-f]{64}",listed[0]):cid=listed[0]
            if cid:
                state=checked_state()
                if state["Running"]:
                    subprocess.run([*engine,"kill",cid],check=True,timeout=15,stdout=subprocess.DEVNULL)
                    subprocess.run([*engine,"wait",cid],check=True,timeout=30,stdout=subprocess.DEVNULL)
                    state=checked_state()
                reaped=not state["Running"] and state["Status"] in ("exited","created")
                if reaped and not gpu_released:
                    gpu_released=recovered_gpu(args.gpu_uuid,baseline)
                if reaped and gpu_released:
                    subprocess.run([*engine,"rm",cid],check=True,timeout=20,stdout=subprocess.DEVNULL)
                    journal.unlink();shutil.rmtree(attempt)
            if journal.exists():
                print("Device attempt retained for reconciliation",journal,file=sys.stderr)
                if reaped:save("recovery_required",cid)
            if receipt is not None:
                require(reaped and gpu_released and not journal.exists(),"Device release is unconfirmed")
                print(json.dumps(receipt),flush=True)


if __name__=="__main__":main()
