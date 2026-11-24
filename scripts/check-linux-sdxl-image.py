#!/usr/bin/env python3.12
"""Verify a content-addressed CUDA candidate, load it locally, and test its actual device/runtime response."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import uuid

ROOT=Path(__file__).resolve().parents[1]
PROFILE="sdxl-base-1.0-euler-fp16-cuda-v1"


def require(ok,reason):
    if not ok:raise ValueError(reason)


def file_sha(path):
    with path.open("rb") as stream:return hashlib.file_digest(stream,"sha256").hexdigest()


def locked_layout(receipt_path):
    manifest=json.loads(receipt_path.read_bytes());root=receipt_path.parent/"layout";blobs=root/"blobs/sha256"
    def blob(item):
        h=item["digest"].removeprefix("sha256:")
        require(len(h)==64 and h.isascii() and all(c in "0123456789abcdef" for c in h),"Invalid descriptor")
        path=blobs/h
        require(path.is_file() and not path.is_symlink() and path.stat().st_size==item["size"] and file_sha(path)==h,"Invalid OCI blob")
        return path
    index=json.loads((root/"index.json").read_bytes());item,=index["manifests"]
    require(item["digest"]==manifest["image"] and item["platform"]=={"architecture":"amd64","os":"linux"},"Unexpected image target")
    doc=json.loads(blob(item).read_bytes());cfg=json.loads(blob(doc["config"]).read_bytes())
    require(doc["config"]["digest"]==manifest["config_digest"] and cfg["config"]["User"]=="65532:65532" and
            cfg["config"]["Entrypoint"]==["/usr/local/bin/python3.12","-I","-B","/opt/orange/app/runner.py"] and cfg["config"]["Labels"]["org.opencontainers.image.revision"]==manifest["source"],
            "Different runtime entry point")
    require(manifest["dependencies"]==72 and len(manifest["offline_installs"])==2 and manifest["offline_installs"][0]==manifest["offline_installs"][1],"Missing independent identical installs")
    all_installs=[manifest["offline_installs"]]
    if "system_installations" in manifest:
        require(len(manifest["system_installations"])==2 and manifest["system_installations"][0]==manifest["system_installations"][1],"Different system layers")
        all_installs.append(manifest["system_installations"])
    if "runner_installations" in manifest:
        pair=manifest["runner_installations"]
        require(len(pair)==2 and pair[0]==pair[1] and pair[0]["runner_sha256"]==file_sha(ROOT/"distribution/sdxl-linux/runner.py"),"Unbound runtime revision")
        all_installs.append(pair)
    for installed,diff,layer in zip(all_installs,cfg["rootfs"]["diff_ids"][-len(all_installs):],doc["layers"][-len(all_installs):],strict=True):
        require(installed[0]["diff_id"]==diff and installed[0]["layer"]==layer["digest"],"Different installed layer")
    inventory={item["digest"][7:],doc["config"]["digest"][7:]}
    for d in doc["layers"]:inventory.add(blob(d).name)
    require(inventory=={p.name for p in blobs.iterdir()},"Unexpected OCI inventory")
    return manifest,root


def main():
    p=argparse.ArgumentParser();p.add_argument("--receipt",type=Path,required=True);p.add_argument("--context",default="colima-orange-agent")
    args=p.parse_args();receipt,root=locked_layout(args.receipt.resolve(strict=True))
    dest=root.parent;output=dest/("orange-sdxl-cuda-amd64-"+receipt["image"][7:23]+".tar")
    # A digest-pinned archive can be imported without network access or source checkout.
    with tempfile.NamedTemporaryFile(dir=dest,delete=False) as temporary:
        stage=Path(temporary.name)
        try:
            with tarfile.open(fileobj=temporary,mode="w",format=tarfile.PAX_FORMAT) as archive:
                for path in sorted(p for p in root.rglob("*") if p.is_file()):
                    info=tarfile.TarInfo(path.relative_to(root).as_posix());info.size=path.stat().st_size;info.mode=0o644;info.mtime=0
                    with path.open("rb") as source:archive.addfile(info,source)
            temporary.flush();checksum=file_sha(stage)
            if output.exists():require(file_sha(output)==checksum,"Previous archive differs")
            else:stage.rename(output)
        finally:
            if stage.exists():stage.unlink()
    engine=["docker","--context",args.context]
    subprocess.run([*engine,"load","-i",output],check=True,timeout=900)
    sysdir=ROOT/"scripts";import sys
    sys.path.insert(0,str(sysdir))
    from linux_platform_inputs import local_image_id
    image=local_image_id(engine,{"image":"orange-dev-input/sdxl-cuda@"+receipt["image"],"config_digest":receipt["config_digest"],"platform":"linux/amd64"})
    require(image,"Imported layout identity mismatch")
    def run(mode,expected,data=None,timeout=180):
        result=subprocess.run([*engine,"run","--rm","-i","--pull=never","--platform","linux/amd64","--network","none","--read-only",
                               "--cap-drop","ALL","--security-opt","no-new-privileges","--user","65532:65532","--pids-limit","128","--memory","2600m",
                               "--tmpfs","/tmp:rw,nosuid,nodev,noexec,mode=1777,size=256m","--tmpfs","/scratch:rw,nosuid,nodev,uid=65532,gid=65532,mode=0700,size=256m",
                               image,mode],input=data,capture_output=True,text=True,timeout=timeout)
        if result.returncode!=expected:raise ValueError("Unexpected "+mode+" result: "+str(result.returncode)+" "+result.stderr[-400:]+" "+result.stdout[-180:])
        return json.loads(result.stdout)
    descriptor=run("describe",0);require(descriptor["model_version"]==PROFILE and descriptor["template"]["device"]=="cuda" and descriptor["wheel_lock_sha256"]==receipt["wheel_lock_sha256"],"Wrong installed profile")
    request={"protocol":1,"model_version":PROFILE,"attempt_id":str(uuid.uuid4()),"prompt":"A single orange on a plate.","seed":34}
    for bad in [dict(request,pipeline="custom"),dict(request,seed=-1),dict(request,prompt="橙"*683),dict(request,model_version="other")]:
        require(run("generate",2,json.dumps(bad))=={"error":"invalid_image_request"},"Unapproved input accepted")
    # No GPU is passed through. A missing shared library or incompatible interpreter is a different failure.
    no_device=run("probe",3,timeout=300)
    require(no_device=={"error":"device_unavailable"},"Backend did not positively separate missing GPU from runtime failure")
    summary={**receipt,"archive":output.name,"sha256":checksum,"bytes":output.stat().st_size,"no_device_denied":True,"malformed_requests_denied":4,
             "cpu_emulated_target":"linux/amd64 CPython 3.12.13","generated_image":False,"public_worker_admission":False}
    (dest/"qualification.json").write_text(json.dumps(summary,indent=2)+"\n")
    print(json.dumps(summary),flush=True)


if __name__=="__main__":main()
