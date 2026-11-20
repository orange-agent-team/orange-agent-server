#!/usr/bin/env python3
"""Qualify the real Rust SDXL host on Apple GPU, including cancellation and atomic output."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
PIN = json.loads((ROOT/"config/local-image.lock.json").read_text())
parser = argparse.ArgumentParser()
parser.add_argument("--model-cache",type=Path,default=ROOT/".local/image-model")
parser.add_argument("--python",type=Path,default=ROOT/".local/image-venv/bin/python")
parser.add_argument("--only",choices=("negative","generate","cancel","all"),default="all")
args=parser.parse_args()
os.umask(0o077)
if not __debug__:
    raise SystemExit("Assertions required for qualification")
cache=args.model_cache.resolve()
python=args.python.absolute()
rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
subprocess.run([cargo,"build","--offline","--locked","-p","orange-model-runtime","--features","image-host","--bin","orange-image"],
               cwd=ROOT,env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"]),check=True)
binary=Path(os.environ.get("CARGO_TARGET_DIR",ROOT/"target")).resolve()/"debug/orange-image"
case={"prompt":"Studio photograph of one ripe orange on a white ceramic plate, soft window light and a pale green background.","seed":7341}
dest=ROOT/".local/image-evidence"
dest.mkdir(mode=0o700,parents=True,exist_ok=True)

with tempfile.TemporaryDirectory(prefix="orange-image-qa-") as temp:
    temp=Path(temp)
    slot=temp/"slot";slot.mkdir(mode=0o700)
    def cmd(target,model=cache):
        return [binary,python,model,slot,target]
    def reject(request,target,message,model=cache):
        result=subprocess.run(cmd(target,model),input=json.dumps(request),text=True,capture_output=True,timeout=90)
        assert result.returncode!=0 and not result.stdout and message in result.stderr, result.stderr[-900:]
    def available():
        with (slot/"sdxl.lock").open("a") as file:
            fcntl.flock(file,fcntl.LOCK_EX|fcntl.LOCK_NB)
        assert list(slot.glob("image-attempt-*"))==[], "private attempt retained after normal shutdown"
    if args.only in ("all","negative"):
        target=temp/"invalid.png"
        for request in [dict(case,steps=1),dict(case,pipeline="custom"),dict(case,prompt=""),dict(case,seed=-1),dict(case,seed=True)]:
            reject(request,target,"Error:")
        with (slot/"sdxl.lock").open("a") as held:
            fcntl.flock(held,fcntl.LOCK_EX)
            reject(case,target,"slot busy")
        reject(dict(case,prompt="orange "*120),target,"encoded prompt length")
        bad=temp/"bad-model";bad.mkdir(mode=0o700)
        # Supply pinned tokenizers then corrupt the next verified regular asset.
        for name in PIN["files"]:
            if name.startswith(("tokenizer/","tokenizer_2/")):
                file=bad/name;file.parent.mkdir(mode=0o700,exist_ok=True);os.link(cache/name,file)
        license_bytes=(cache/"LICENSE.md").read_bytes()
        (bad/"LICENSE.md").write_bytes(b"x"+license_bytes[1:])
        reject(case,target,"backend unavailable",bad)
        assert not target.exists()
        target.write_bytes(b"existing destination")
        reject(case,target,"destination already exists")
        assert target.read_bytes()==b"existing destination"
        available()
        print("PASS fixed request, excess CLIP tokens, model mutation, exclusive device slot and existing output protection",flush=True)
    def run_one(name,stop):
        output=temp/(name+".png")
        start=time.monotonic()
        progress=temp/(name+".log")
        with progress.open("wb") as err, subprocess.Popen(cmd(output),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=err) as proc:
            try:
                proc.stdin.write(json.dumps(case).encode());proc.stdin.close()
                cancelled=False
                end=time.monotonic()+1810
                while proc.poll() is None:
                    if time.monotonic()>end:raise TimeoutError("Owned image operation took too long")
                    if stop and not cancelled and b'"completed_steps":1,' in progress.read_bytes():
                        reject(case,temp/"busy.png","slot busy")
                        proc.send_signal(signal.SIGINT);cancelled=True
                    time.sleep(.1)
                payload=proc.stdout.read()
            finally:
                if proc.poll() is None:
                    proc.send_signal(signal.SIGTERM)
                    try:proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:proc.kill();proc.wait()
                shutil.copy2(progress,dest/(name+"-progress.log"))
        available()
        if stop:
            assert cancelled and proc.returncode!=0 and not payload and not output.exists(),progress.read_text()[-800:]
            return {"cancelled_after_actual_step":True,"slot_reacquired":True,"final_png":False}
        if proc.returncode:raise RuntimeError(progress.read_text()[-1500:])
        receipt=json.loads(payload);data=output.read_bytes()
        assert len(data)==receipt["bytes"] and hashlib.sha256(data).hexdigest()==receipt["sha256"]
        assert (receipt["steps"],receipt["width"],receipt["height"],receipt["seed"])==(25,1024,1024,case["seed"])
        assert receipt["observed_driver_bytes"]>=receipt["observed_tensor_bytes"]>0
        final=dest/(receipt["sha256"]+".png")
        if not final.exists():shutil.copy2(output,final)
        assert final.read_bytes()==data
        receipt["elapsed_seconds"]=round(time.monotonic()-start,3)
        return receipt
    results={"model_version":PIN["model_version"],"scope":"macOS arm64 actual local MPS and fixed SDXL base; no public GPU, priced Job or CUDA certification"}
    if args.only in ("all","generate"):
        images=[run_one("generate-"+str(i),False) for i in range(2)]
        results.update(images=images,identical_rgb=images[0]["rgb_sha256"]==images[1]["rgb_sha256"])
    if args.only in ("all","cancel"):
        results["cancellation"]=run_one("cancel",True)
    (dest/(args.only+".json")).write_text(json.dumps(results,indent=2)+"\n")
    print(json.dumps(results),flush=True)
