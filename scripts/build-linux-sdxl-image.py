#!/usr/bin/env python3.12
"""Produce a deterministic OCI layout for the offline linux/amd64 CUDA SDXL adapter."""
import argparse
from contextlib import ExitStack
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import uuid

from linux_platform_inputs import local_image_id

ROOT=Path(__file__).resolve().parents[1]
ENVROOT=ROOT/"backends/environments/sdxl-linux-cuda"
MANIFEST="application/vnd.oci.image.manifest.v1+json"
CONFIG="application/vnd.oci.image.config.v1+json"
LAYER="application/vnd.oci.image.layer.v1.tar+gzip"

def require(ok,message):
    if not ok:raise ValueError(message)

def encoded(obj):
    return json.dumps(obj,sort_keys=True,separators=(",",":"),ensure_ascii=False).encode()

def file_sha(path):
    with path.open("rb") as stream:return hashlib.file_digest(stream,"sha256").hexdigest()

def raw_sha(data):return hashlib.sha256(data).hexdigest()

def member(name,size=0,mode=0o644):
    meta=tarfile.TarInfo(name);meta.size=size;meta.mode=mode;meta.mtime=0;meta.uid=meta.gid=0;meta.uname=meta.gname=""
    return meta

class HashWriter:
    def __init__(self,stream):self.stream=stream;self.digest=hashlib.sha256()
    def write(self,raw):self.digest.update(raw);return self.stream.write(raw)
    def flush(self):self.stream.flush()

def layer(site,output):
    seen=0
    with output.open("xb") as raw,gzip.GzipFile(fileobj=raw,mode="wb",mtime=0,filename="",compresslevel=1) as zipped:
        stream=HashWriter(zipped)
        with tarfile.open(mode="w|",fileobj=stream,format=tarfile.PAX_FORMAT) as tar:
            for directory in ("opt","opt/orange"):
                info=member(directory,mode=0o755);info.type=tarfile.DIRTYPE;tar.addfile(info)
            for path in sorted(site.rglob("*"),key=lambda f:f.relative_to(site).as_posix()):
                rel="opt/orange/"+path.relative_to(site).as_posix();meta=path.lstat()
                if stat.S_ISDIR(meta.st_mode):
                    info=member(rel,mode=0o755);info.type=tarfile.DIRTYPE;tar.addfile(info)
                elif stat.S_ISREG(meta.st_mode):
                    info=member(rel,meta.st_size,0o755 if meta.st_mode&0o111 else 0o644)
                    with path.open("rb") as src:tar.addfile(info,src)
                    seen+=1
                elif stat.S_ISLNK(meta.st_mode):
                    target=os.readlink(path)
                    require(not Path(target).is_absolute() and (path.parent/target).resolve(strict=True).is_relative_to(site.resolve()),"Nonlocal installed link")
                    info=member(rel,mode=0o777);info.type=tarfile.SYMTYPE;info.linkname=target;tar.addfile(info)
                else:raise ValueError("Unexpected installed file kind")
    return stream.digest.hexdigest(),seen

def transfer(docker,cid,cache,selected,requirements,install_script):
    # The VM has no host filesystem share. Supply only verified wheels and this fixed installer.
    with subprocess.Popen([*docker,"cp","-",(cid+":/tmp")],stdin=subprocess.PIPE) as proc:
        try:
            with tarfile.open(fileobj=proc.stdin,mode="w|") as bundle:
                info=member("input",mode=0o755);info.type=tarfile.DIRTYPE;bundle.addfile(info)
                for name,data in (("requirements.txt",requirements),("install.py",install_script)):
                    bundle.addfile(member("input/"+name,len(data)),io.BytesIO(data))
                for item in selected:
                    path=cache/item["filename"]
                    require(path.parent==cache and not path.is_symlink() and path.stat().st_size==item["bytes"] and file_sha(path)==item["sha256"],"Invalid cached wheel "+item["package"])
                    with path.open("rb") as stream:bundle.addfile(member("input/"+path.name,item["bytes"]),stream)
            proc.stdin.close();require(proc.wait()==0,"Offline wheel transfer failed")
        finally:
            if not proc.stdin.closed:proc.stdin.close()

def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--wheel-cache",type=Path,default=ROOT/".local/linux-sdxl-wheelhouse")
    cli.add_argument("--base-cache",type=Path,default=ROOT/".local/base-oci")
    cli.add_argument("--output",type=Path,default=ROOT/".local/sdxl-oci")
    cli.add_argument("--context",default="colima-orange-agent")
    args=cli.parse_args();cache=args.wheel_cache.resolve(strict=True)
    base=json.loads((ENVROOT/"base-image.lock.json").read_bytes());raw=(ENVROOT/"base-manifest.json").read_bytes()
    require("sha256:"+raw_sha(raw)==base["manifest_digest"],"Unpinned base manifest")
    base_manifest=json.loads(raw);require(base_manifest["config"]["digest"]==base["config_digest"],"Changed base config")
    prefix="sha256:";cfgfile=args.base_cache/"blobs/sha256"/base["config_digest"].removeprefix(prefix)
    require(file_sha(cfgfile)==cfgfile.name,"Changed base config blob")
    config=json.loads(cfgfile.read_bytes());require(config["os"]=="linux" and config["architecture"]=="amd64","Incorrect platform")
    read=lambda name:json.loads((ENVROOT/name).read_bytes())
    lock=read("wheels.lock.json");require(file_sha(ENVROOT/"uv.lock")==lock["uv_lock_sha256"],"Wheel lock mismatch")
    source=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    require(not subprocess.check_output(["git","status","--porcelain"],cwd=ROOT),"Clean committed source required")
    docker=["docker","--context",args.context]
    image=local_image_id(docker,dict(image=base["repository"]+"@"+base["manifest_digest"],config_digest=base["config_digest"],platform=base["platform"]))
    require(image,"Prepare the pinned Python image using prepare-linux-images.py --image sdxl_python")
    dest=args.output.resolve()/source;dest.mkdir(parents=True,exist_ok=True)
    layout=dest/"layout";blobs=layout/"blobs/sha256";blobs.mkdir(parents=True,exist_ok=True)
    def retain(path,digest):
        saved=blobs/digest
        if saved.is_file():require(file_sha(saved)==digest,"Conflicting OCI blob");return
        # Source and destination are both private caches and remain content-addressed.
        try:os.link(path,saved)
        except OSError:shutil.copyfile(path,saved)
        require(file_sha(saved)==digest,"OCI transfer differs")
    for entry in base_manifest["layers"]:
        src=args.base_cache/"blobs/sha256"/entry["digest"].removeprefix(prefix)
        require(src.stat().st_size==entry["size"] and file_sha(src)==src.name,"Missing base layer")
        retain(src,src.name)
    requirements="".join(f"{item['package']}=={item['version']} --hash=sha256:{item['sha256']}\n" for item in lock["wheels"]).encode()
    install=(ROOT/"distribution/sdxl-linux/install.py").read_bytes()
    runs=[]
    for number in (1,2):
        with tempfile.TemporaryDirectory(prefix=".build-",dir=dest) as tmp,ExitStack() as cleanup:
            tmp=Path(tmp);nonce=uuid.uuid4().hex
            cmd=[*docker,"create","--pull=never","--platform","linux/amd64","--network","none","--cap-drop","ALL","--security-opt","no-new-privileges",
                 "--user","65532:65532","--pids-limit","128","--memory","4g","--cpus","3","--label","orange.owned="+nonce,
                 "-e","HOME=/tmp","-e","SOURCE_DATE_EPOCH=946684800","-e","PYTHONDONTWRITEBYTECODE=1",image,"python","-I","-B","/tmp/input/install.py"]
            cid=subprocess.check_output(cmd,text=True).strip()
            def dispose():
                owner=subprocess.check_output([*docker,"inspect","--format",'{{index .Config.Labels "orange.owned"}}',cid],text=True).strip()
                if owner==nonce:subprocess.run([*docker,"rm","-f",cid],check=True,stdout=subprocess.DEVNULL)
            cleanup.callback(dispose)
            transfer(docker,cid,cache,lock["wheels"],requirements,install)
            subprocess.run([*docker,"start","-a",cid],check=True,timeout=1800)
            state=subprocess.check_output([*docker,"inspect","--format","{{.State.ExitCode}}",cid],text=True).strip()
            require(state=="0","Pinned target installer failed")
            site=tmp/"tree";site.mkdir()
            subprocess.run([*docker,"cp",cid+":/tmp/output/.",site],check=True,timeout=900)
            # All runtime content comes from these immutable tracked inputs.
            app=site/"app";app.mkdir()
            for src,name in [(ROOT/"distribution/sdxl-linux/runner.py","runner.py"),(ROOT/"backends/orange_inference_backends/image.py","model_assets.py"),
                             (ROOT/"config/local-image.lock.json","model.lock.json"),(ENVROOT/"base-image.lock.json","base-image.lock.json"),(ENVROOT/"wheels.lock.json","wheels.lock.json")]:
                (app/name).write_bytes(src.read_bytes())
            archive=tmp/"runtime.tar.gz";diff,count=layer(site,archive);content=file_sha(archive);size=archive.stat().st_size
            if not runs:retain(archive,content)
            runs.append({"diff_id":prefix+diff,"layer":prefix+content,"bytes":size,"regular_files":count})
            print("Independent target installation",number,json.dumps(runs[-1]),flush=True)
    require(runs[0]==runs[1],"Independent installed layers differ")
    r=runs[0];config["rootfs"]["diff_ids"].append(r["diff_id"])
    config["history"].append({"created":"2000-01-01T00:00:00Z","created_by":"Orange locked offline SDXL environment"})
    config["created"]="2000-01-01T00:00:00Z"
    config["config"]={"User":"65532:65532","Entrypoint":["/usr/local/bin/python3.12","-I","-B","/opt/orange/app/runner.py"],"Cmd":["describe"],
                      "WorkingDir":"/tmp","Env":["PATH=/usr/local/bin:/usr/bin:/bin","HOME=/tmp","HF_HUB_OFFLINE=1","TRANSFORMERS_OFFLINE=1","HF_HUB_DISABLE_TELEMETRY=1","TOKENIZERS_PARALLELISM=false","TMPDIR=/scratch"],
                      "Labels":{"org.opencontainers.image.revision":source,"org.opencontainers.image.title":"Orange fixed CUDA SDXL development runner"}}
    def put(data,kind):
        h=raw_sha(data);f=blobs/h
        if not f.exists():f.write_bytes(data)
        require(file_sha(f)==h,"Conflicting metadata")
        return {"mediaType":kind,"digest":prefix+h,"size":len(data)}
    cfg=put(encoded(config),CONFIG)
    manifest={"schemaVersion":2,"mediaType":MANIFEST,"config":cfg,"layers":[*base_manifest["layers"],{"mediaType":LAYER,"digest":r["layer"],"size":r["bytes"]}]}
    md=put(encoded(manifest),MANIFEST)
    tag="orange-dev-input/sdxl-cuda:"+source[:12]
    (layout/"oci-layout").write_bytes(encoded({"imageLayoutVersion":"1.0.0"}))
    (layout/"index.json").write_bytes(encoded({"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{**md,"platform":{"os":"linux","architecture":"amd64"},
                                    "annotations":{"org.opencontainers.image.ref.name":tag}}]}))
    report={"source":source,"platform":"linux/amd64","image":md["digest"],"config_digest":cfg["digest"],"local_tag":tag,"base_digest":base["manifest_digest"],
            "wheel_lock_sha256":file_sha(ENVROOT/"wheels.lock.json"),"dependencies":len(lock["wheels"]),"offline_installs":runs,"hardware_certified":False,"published":False}
    (dest/"receipt.json").write_bytes(encoded(report)+b"\n");print(json.dumps(report),flush=True)

if __name__=="__main__":main()
