#!/usr/bin/env python3.12
"""Package the qualified single-device supervisor and its offline Linux AMD64 Pillow dependency."""
import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile
import zipfile

ROOT=Path(__file__).resolve().parents[1]
FILES=("scripts/benchmark-sdxl-device.py","scripts/check-sdxl-target-benchmark.py","scripts/check-sdxl-device-recovery.py",
       "scripts/run-linux-sdxl-attempt.py","scripts/sdxl_device_recovery.py","scripts/sdxl_progress.py","scripts/sdxl_png.py","scripts/linux_platform_inputs.py",
       "distribution/sdxl-linux/runner.py","distribution/sdxl-linux/model-version.json","distribution/sdxl-linux/README.md","distribution/sdxl-linux/qualification.json")


def build(entries):
    blob=io.BytesIO()
    with gzip.GzipFile(fileobj=blob,mode="wb",filename="",mtime=0) as zipped:
        with tarfile.open(fileobj=zipped,mode="w",format=tarfile.USTAR_FORMAT) as tar:
            for name,data in sorted(entries.items()):
                info=tarfile.TarInfo(name)
                info.size=len(data)
                info.mode=0o644
                tar.addfile(info,io.BytesIO(data))
    return blob.getvalue()


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--wheel-cache",type=Path,default=ROOT/".local/linux-sdxl-wheelhouse")
    parser.add_argument("--output",required=True,type=Path)
    args=parser.parse_args()
    if subprocess.check_output(["git","status","--porcelain"],cwd=ROOT):
        raise RuntimeError("Expected clean server checkout")
    revision=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    def source(name):
        return subprocess.check_output(["git","show",revision+":"+name],cwd=ROOT)
    lock=json.loads(source("backends/environments/sdxl-linux-cuda/wheels.lock.json"))
    pillow,=(row for row in lock["wheels"] if row["package"]=="pillow")
    path=args.wheel_cache/pillow["filename"]
    data=path.read_bytes()
    if path.is_symlink() or len(data)!=pillow["bytes"] or hashlib.sha256(data).hexdigest()!=pillow["sha256"]:
        raise ValueError("Pillow target wheel differs from fixed release")
    entries={name:source(name) for name in FILES}
    entries["wheelhouse/"+path.name]=data
    entries["requirements.txt"]=f'pillow=={pillow["version"]} --hash=sha256:{pillow["sha256"]}\n'.encode()
    with zipfile.ZipFile(io.BytesIO(data)) as wheel:
        notices=sorted(p for p in wheel.namelist() if ".dist-info/licenses/" in p and not p.endswith("/"))
        if not notices:raise ValueError("Pinned Pillow license texts absent")
        for item in notices:
            entries["notices/"+item.split(".dist-info/licenses/",1)[1]]=wheel.read(item)
    authority=entries["distribution/sdxl-linux/qualification.json"]
    q=json.loads(authority)
    draft=entries["distribution/sdxl-linux/model-version.json"]
    if json.loads(draft)["runtime"]!={"engine":"diffusers","image_digest":q["image"]}:
        raise ValueError("Packaged draft differs from the qualified runtime")
    entries["source.json"]=json.dumps({"server":revision,"image_source":q["source"],"image":q["image"],
        "model_version_file_sha256":hashlib.sha256(draft).hexdigest(),"qualification_sha256":hashlib.sha256(authority).hexdigest(),"pillow_sha256":pillow["sha256"],"hardware_certified":False},sort_keys=True).encode()
    first=build(entries)
    if first!=build(entries):raise RuntimeError("Two reproducible builds differ")
    digest=hashlib.sha256(first).hexdigest()
    args.output.mkdir(parents=True,exist_ok=True)
    target=args.output/("orange-sdxl-target-kit-linux-amd64-"+digest[:16]+".tar.gz")
    if not target.exists():
        with target.open("xb") as out:out.write(first)
    if target.is_symlink() or target.read_bytes()!=first:raise RuntimeError("Conflicting target kit")
    print(json.dumps({"source":revision,"bytes":len(first),"sha256":digest,"archive":target.name,"independent_builds":2,
                      "notice_files":len(notices),"image":q["image"],"qualification_sha256":hashlib.sha256(authority).hexdigest()}))


if __name__=="__main__":
    main()
