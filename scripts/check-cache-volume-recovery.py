#!/usr/bin/env python3.12
"""Force a real ENOSPC on a small private macOS disk image, then recover via the installed cache CLI."""
import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tempfile
import time

ROOT=Path(__file__).resolve().parents[1]
MIB=1024*1024
HDI="/usr/bin/hdiutil"


def check(ok,reason):
    if not ok:raise ValueError(reason)


def attachments(image):
    raw=subprocess.check_output([HDI,"info","-plist"],timeout=30)
    return [e for item in plistlib.loads(raw)["images"] if Path(item["image-path"]).resolve()==image
            for e in item.get("system-entities",[]) if e.get("dev-entry")]


@contextmanager
def private_volume():
    if platform.system()!="Darwin":raise RuntimeError("macOS hdiutil is required for this physical ENOSPC qualifier")
    temp=Path(tempfile.mkdtemp(prefix="oa-cache-volume-")).resolve()
    image=temp/"bounded.dmg";mount=temp/"mount";mount.mkdir(mode=0o700)
    try:
        subprocess.run([HDI,"create","-quiet","-size","24m","-layout","NONE","-fs","HFS+","-volname","OrangeCacheQA",image],check=True,timeout=60)
        subprocess.run([HDI,"attach","-plist","-nobrowse","-noautoopen","-owners","on","-mountpoint",mount,image],check=True,stdout=subprocess.PIPE,timeout=60)
        bound=[e for e in attachments(image) if Path(e.get("mount-point","")).resolve()==mount]
        check(len(bound)==1 and os.path.ismount(mount) and mount.stat().st_dev!=temp.stat().st_dev,"Test mount is not the owned disk image")
        meta=os.statvfs(mount)
        check(12*MIB<=meta.f_blocks*meta.f_frsize<=32*MIB,"Refusing to fill a different filesystem")
        yield temp,mount
    finally:
        if image.is_file():
            # Only act on the exact backing image we just created. Leave it intact if detach is uncertain.
            for entity in attachments(image):
                if entity.get("mount-point")==str(mount):
                    for attempt in range(3):
                        detached=subprocess.run([HDI,"detach",entity["dev-entry"]],capture_output=True,timeout=30)
                        if detached.returncode==0:break
                        time.sleep(1)
                    else:raise RuntimeError("Owned image retained after failed detach: "+str(image))
            check(not attachments(image) and not os.path.ismount(mount),"Retained still-attached test image "+str(image))
        shutil.rmtree(temp)


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--binary",type=Path)
    parser.add_argument("--sha256")
    args=parser.parse_args()
    if bool(args.binary)!=bool(args.sha256):parser.error("--binary and --sha256 are required together")
    if args.binary:
        program=args.binary.resolve(strict=True)
        with program.open("rb") as stream:check(hashlib.file_digest(stream,"sha256").hexdigest()==args.sha256,"Wrong binary")
    else:
        rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
        cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
        env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
        subprocess.run([cargo,"build","--locked","--offline","-p","orange-model-runtime","--bin","model-fetch"],cwd=ROOT,env=env,check=True)
        target=json.loads(subprocess.check_output([cargo,"metadata","--locked","--offline","--no-deps","--format-version","1"],cwd=ROOT,env=env))["target_directory"]
        program=Path(target)/"debug/model-fetch"
    with private_volume() as (temp,mount):
        cache=mount/"cache";cache.mkdir(mode=0o700);source=temp/"selected.gguf"
        data=b"GGUF"+bytes(range(256))*(8*MIB//256)
        source.write_bytes(data);digest=hashlib.sha256(data).hexdigest()
        manifest=temp/"fixture.json"
        manifest.write_text(json.dumps({"model":{"repository":"orange/fixture","revision":"1"*40,"file":"model.gguf","bytes":len(data),"sha256":digest}}))
        space=os.statvfs(mount);reserve_size=(space.f_bavail*space.f_frsize)-3*MIB
        check(2*MIB<=reserve_size<=26*MIB,"Unexpected available capacity on bounded test image")
        reserve=mount/"owned-reservation"
        with reserve.open("xb") as f:
            chunk=b"X"*MIB
            while reserve_size>0:
                n=min(MIB,reserve_size);f.write(chunk[:n]);reserve_size-=n
            f.flush();os.fsync(f.fileno())
        def cli(*flags):return subprocess.run([program,manifest,cache,*flags],capture_output=True,text=True,timeout=25)
        failed=cli("--import-file",source)
        check(failed.returncode!=0 and not failed.stdout and ("No space left on device" in failed.stderr or "StorageFull" in failed.stderr),
              "Import did not surface ENOSPC: "+failed.stderr[:200])
        state=json.loads(cli("--inspect").stdout)
        check(state["state"]=="staged" and 0<=state["import_staging_bytes"]<len(data) and not list(cache.glob("*.gguf")),"Unverified data published after disk exhaustion")
        cleanup=json.loads(cli("--discard-staging").stdout)
        check(cleanup=={"state":"released","import_bytes":state["import_staging_bytes"],"download_bytes":0} and not list(cache.glob("*.import")),
              "Failed to reclaim partial import on a full filesystem")
        reserve.unlink()
        imported=cli("--import-file",source)
        check(imported.returncode==0 and json.loads(imported.stdout)["sha256"]==digest, "Import did not recover")
        check(json.loads(cli("--inspect").stdout)["state"]=="verified" and len(list(cache.glob("*.gguf")))==1,"Missing verified publication")
        check(json.loads(cli("--discard-staging").stdout)["import_bytes"]==0 and hashlib.sha256((cache/(digest+".gguf")).read_bytes()).hexdigest()==digest,"Staging cleanup removed verified bytes")
    print(json.dumps({"real_enospc":True,"partial_published":0,"bytes_reclaimed":cleanup["import_bytes"],
                      "recovered_verified_bytes":len(data),"private_volume_detached":True}),flush=True)


if __name__=="__main__":main()
