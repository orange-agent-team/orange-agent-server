#!/usr/bin/env python3
"""Import content-addressed official Linux images into a local Docker engine, with no guest host mounts."""
from linux_platform_inputs import local_image_id
import argparse
from concurrent.futures import ThreadPoolExecutor
import fcntl
import http.client
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT=Path(__file__).resolve().parents[1]
LOCK=ROOT/"config/linux-platform-arm64.lock.json"
INDEX_TYPE="application/vnd.oci.image.index.v1+json"
MANIFEST_TYPES={"application/vnd.oci.image.manifest.v1+json","application/vnd.docker.distribution.manifest.v2+json"}

class RegistryRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        target=urllib.parse.urlsplit(newurl)
        if target.scheme!="https" or not target.hostname or target.username or target.password:
            raise ValueError("Invalid registry transport redirect")
        forwarded=super().redirect_request(req,fp,code,msg,headers,newurl)
        if forwarded is not None and target.netloc!=urllib.parse.urlsplit(req.full_url).netloc:
            forwarded.remove_header("Authorization")
        return forwarded

def digest(value):
    if not re.fullmatch(r"sha256:[0-9a-f]{64}",value):
        raise ValueError("Expected SHA-256 digest")
    return value[7:]

def sha(path):
    value=hashlib.sha256()
    with path.open("rb") as stream:
        for part in iter(lambda:stream.read(1024*1024),b""):value.update(part)
    return value.hexdigest()

def locked_blob(opener,url,token,descriptor,directory):
    target=directory/digest(descriptor["digest"]);size=descriptor["size"]
    if not isinstance(size,int) or size<=0:raise ValueError("Invalid blob size")
    if target.is_file() and not target.is_symlink() and target.stat().st_size==size and sha(target)==target.name:
        return target
    temp=target.with_suffix(".partial")
    if temp.is_symlink() or target.is_symlink():raise ValueError("Linked build input refused")
    for attempt in range(3):
        offset=temp.stat().st_size if temp.exists() else 0
        if offset==size and sha(temp)==target.name:
            os.replace(temp,target);return target
        if offset>=size:
            temp.write_bytes(b"");offset=0
        req=urllib.request.Request(url,headers={"Accept-Encoding":"identity","User-Agent":"orange-local-build"})
        req.add_unredirected_header("Authorization","Bearer "+token)
        if offset:req.add_header("Range",f"bytes={offset}-")
        try:
            with opener.open(req,timeout=60) as response:
                if response.status==200:offset=0
                elif response.status!=206 or response.headers.get("Content-Range")!=f"bytes {offset}-{size-1}/{size}":
                    raise ValueError("Unexpected registry range")
                with temp.open("ab" if offset else "wb") as output:
                    while part:=response.read(1024*1024):
                        if output.tell()+len(part)>size:raise ValueError("Oversized registry blob")
                        output.write(part)
            if temp.stat().st_size!=size or sha(temp)!=target.name:raise ValueError("Registry content digest mismatch")
            os.replace(temp,target);return target
        except (OSError, http.client.IncompleteRead, ValueError) as exc:
            if attempt==2:
                raise RuntimeError(f"Could not verify blob {target.name}: {type(exc).__name__}, HTTP {getattr(exc,'code',None)}") from None
            time.sleep(1<<attempt)
    raise RuntimeError("Registry fetch exhausted")

def read_registry(opener,req,limit):
    for attempt in range(3):
        try:
            with opener.open(req,timeout=35) as response:
                data=response.read(limit+1)
            if len(data)>limit:
                raise ValueError("Registry metadata exceeds its limit")
            return data
        except (OSError,http.client.IncompleteRead) as exc:
            if attempt==2:
                raise RuntimeError(f"Registry metadata unavailable: {type(exc).__name__}, HTTP {getattr(exc,'code',None)}") from None
            time.sleep(1<<attempt)

def import_image(docker,entry,cache):
    ref=entry["image"]
    match=re.fullmatch(r"docker.io/library/([a-z0-9-]+)@(sha256:[0-9a-f]{64})",ref)
    if not match:raise ValueError("Expected pinned Docker Official Image")
    repo,manifest_hash=match.groups();config=entry["config_digest"];digest(config)
    platform=entry.get("platform","linux/arm64")
    if platform not in ("linux/arm64","linux/amd64"):raise ValueError("Unsupported platform")
    architecture=platform.split("/")[1]
    if local_image_id(docker,entry):
        print("Verified local image",repo,manifest_hash,flush=True);return
    opener=urllib.request.build_opener(RegistryRedirect())
    auth="https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/"+repo+":pull"
    token=json.loads(read_registry(opener,auth,131072))["token"]
    base="https://registry-1.docker.io/v2/library/"+repo
    req=urllib.request.Request(base+"/manifests/"+manifest_hash,headers={"Accept":", ".join(sorted(MANIFEST_TYPES))})
    req.add_unredirected_header("Authorization","Bearer "+token)
    raw=read_registry(opener,req,1024*1024)
    if hashlib.sha256(raw).hexdigest()!=digest(manifest_hash):raise ValueError("Manifest digest mismatch")
    manifest=json.loads(raw);media=manifest["mediaType"]
    if media not in MANIFEST_TYPES or manifest["config"]["digest"]!=config:raise ValueError("Unexpected image manifest")
    blobs=cache/"blobs"/"sha256";blobs.mkdir(parents=True,exist_ok=True)
    descriptor=[manifest["config"],*manifest["layers"]]
    sources=list(dict((d["digest"],d) for d in descriptor).values())
    print("Fetching official",repo,len(sources),"blobs",sum(x["size"] for x in sources),"bytes",flush=True)
    with ThreadPoolExecutor(max_workers=2) as pool:
        files=list(pool.map(lambda d:locked_blob(opener,base+"/blobs/"+d["digest"],token,d,blobs),sources))
    identity=json.loads((blobs/digest(config)).read_bytes())
    if identity.get("os")!="linux" or identity.get("architecture")!=architecture:
        raise ValueError("Base config does not match the locked target")
    display="docker.io/orange-dev-input/"+repo+":"+digest(manifest_hash)[:16]
    desc={"mediaType":media,"digest":manifest_hash,"size":len(raw),"annotations":{"org.opencontainers.image.ref.name":display},"platform":{"architecture":architecture,"os":"linux"}}
    def member(tar,name,data):
        meta=tarfile.TarInfo(name);meta.size=len(data);meta.mode=0o644;tar.addfile(meta,io.BytesIO(data))
    with tempfile.TemporaryDirectory(prefix="image-import-",dir=cache) as tmp:
        archive=Path(tmp)/"image.tar"
        with tarfile.open(archive,"w") as tar:
            member(tar,"oci-layout",b'{"imageLayoutVersion":"1.0.0"}')
            member(tar,"index.json",json.dumps({"schemaVersion":2,"mediaType":INDEX_TYPE,"manifests":[desc]}).encode())
            member(tar,"blobs/sha256/"+digest(manifest_hash),raw)
            for path in files:tar.add(path,"blobs/sha256/"+path.name,recursive=False)
        subprocess.run(docker+["load","-i",str(archive)],check=True)
    if not local_image_id(docker,entry):raise ValueError("Imported configuration or platform differs from lock")
    print("Verified imported image",ref,flush=True)

if __name__=="__main__":
    parser=argparse.ArgumentParser()
    parser.add_argument("--context",default="colima-orange-agent")
    parser.add_argument("--cache",type=Path,default=ROOT/".local/linux/oci")
    parser.add_argument("--image",choices=["build_image","runtime_image","test_database","development_cache","sdxl_python"],action="append")
    args=parser.parse_args();lock=json.loads(LOCK.read_bytes())
    if "development_cache" in (args.image or []):
        lock["development_cache"]=json.loads((ROOT/"config/development-cache.lock.json").read_bytes())
    if "sdxl_python" in (args.image or []):
        base=json.loads((ROOT/"backends/environments/sdxl-linux-cuda/base-image.lock.json").read_bytes())
        lock["sdxl_python"]={"image":base["repository"]+"@"+base["manifest_digest"],"config_digest":base["config_digest"],"platform":base["platform"]}
    args.cache.mkdir(mode=0o700,parents=True,exist_ok=True)
    with (args.cache/".import.lock").open("a") as lockfile:
        fcntl.flock(lockfile,fcntl.LOCK_EX)
        for key in args.image or ["build_image","runtime_image"]:
            import_image(["docker","--context",args.context],lock[key],args.cache.resolve())
