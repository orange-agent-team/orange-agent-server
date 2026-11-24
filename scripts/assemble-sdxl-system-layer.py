#!/usr/bin/env python3.12
"""Complete the locked image with the target APT resolver's signed Debian runtime files and notices."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path,PurePosixPath
import posixpath
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request

ROOT=Path(__file__).resolve().parents[1]
LOCK=ROOT/"backends/environments/sdxl-linux-cuda/debian-libs.lock.json"
PARENT="cdad5d375fd394251a29a062e0311c2696de917c"


def require(value,why):
    if not value:raise ValueError(why)


def sha(path):
    with path.open("rb") as f:return hashlib.file_digest(f,"sha256").hexdigest()


def canonical(v):return json.dumps(v,sort_keys=True,separators=(",",":")).encode()


def overlay(lock,cache,scratch,out):
    tree=scratch/"files";tree.mkdir()
    receipts={}
    for pkg in lock["packages"]:
        deb=cache/Path(pkg["path"]).name
        require(not deb.is_symlink() and deb.stat().st_size==pkg["bytes"] and sha(deb)==pkg["sha256"],"Unverified Debian package "+pkg["name"])
        archive,=[a for a in subprocess.check_output(["ar","-t",deb],text=True).splitlines() if a.startswith("data.tar.")]
        raw=subprocess.check_output(["ar","-p",deb,archive])
        with tarfile.open(fileobj=io.BytesIO(raw)) as payload:
            for member in payload:
                n=member.name.removeprefix("./").rstrip("/")
                if not n or n==".":continue
                rel=PurePosixPath(n)
                require(not rel.is_absolute() and ".." not in rel.parts and n==rel.as_posix(),"Unexpected package path")
                require(rel.parts[0] in ("usr","etc"),"Unexpected runtime root")
                dest=tree/n
                require(dest.parent.resolve().is_relative_to(tree),"Escaping archive parent")
                if member.isdir():dest.mkdir(parents=True,exist_ok=True);continue
                dest.parent.mkdir(parents=True,exist_ok=True)
                if os.path.lexists(dest):raise ValueError("Overlapping Debian file "+n)
                if member.issym():
                    t=member.linkname;final=posixpath.normpath(posixpath.join(posixpath.dirname(n),t))
                    require(not t.startswith("/") and not final.startswith("../"),"Nonlocal Debian link")
                    dest.symlink_to(t)
                elif member.islnk():
                    target=tree/member.linkname.removeprefix("./")
                    require(target.is_file() and not target.is_symlink() and target.resolve().is_relative_to(tree),"Unexpected package hard link")
                    os.link(target,dest)
                else:
                    require(member.isfile() and not member.mode&0o6000,"Unexpected package file kind")
                    with payload.extractfile(member) as src,dest.open("xb") as sink:shutil.copyfileobj(src,sink)
                    dest.chmod(0o755 if member.mode&0o111 else 0o644)
        require((tree/"usr/share/doc"/pkg["name"]/"copyright").resolve().is_file(),"Missing package notice "+pkg["name"])
        receipts[pkg["name"]]=pkg["version"]
    app=tree/"opt/orange/app";app.mkdir(parents=True)
    (app/"debian-libs.lock.json").write_bytes(LOCK.read_bytes())
    # Normalize ownership, file times and ordering; retain only safe existing in-tree hardlinks.
    contents=hashlib.sha256()
    class Dual:
        def __init__(self,f):self.f=f
        def write(self,b):contents.update(b);return self.f.write(b)
        def flush(self):self.f.flush()
    seen={}
    with out.open("xb") as file,gzip.GzipFile(fileobj=file,mode="wb",filename="",mtime=0,compresslevel=1) as compressed,tarfile.open(mode="w|",fileobj=Dual(compressed),format=tarfile.PAX_FORMAT) as archive:
        for path in sorted(tree.rglob("*"),key=lambda p:p.relative_to(tree).as_posix()):
            n=path.relative_to(tree).as_posix();entry=tarfile.TarInfo(n);entry.mtime=0
            if path.is_symlink():entry.type=tarfile.SYMTYPE;entry.linkname=os.readlink(path);entry.mode=0o777
            elif path.is_dir():entry.type=tarfile.DIRTYPE;entry.mode=0o755
            else:
                meta=path.stat();inode=(meta.st_dev,meta.st_ino);entry.mode=0o755 if meta.st_mode&0o111 else 0o644
                if inode in seen:entry.type=tarfile.LNKTYPE;entry.linkname=seen[inode]
                else:
                    seen[inode]=n;entry.size=meta.st_size
                    with path.open("rb") as src:archive.addfile(entry,src)
                    continue
            archive.addfile(entry)
    return {"layer":"sha256:"+sha(out),"diff_id":"sha256:"+contents.hexdigest(),"bytes":out.stat().st_size,"packages":len(receipts)}


def main():
    cli=argparse.ArgumentParser();cli.add_argument("--parent-receipt",type=Path,required=True);cli.add_argument("--packages",type=Path,default=ROOT/".local/debian-inputs")
    cli.add_argument("--output",type=Path,default=ROOT/".local/sdxl-oci");cli.add_argument("--fetch-locked",action="store_true")
    a=cli.parse_args();cache=a.packages.resolve();cache.mkdir(exist_ok=True,parents=True)
    source=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    require(not subprocess.check_output(["git","status","--porcelain"],cwd=ROOT),"Clean source required")
    receipt=json.loads(a.parent_receipt.read_bytes());lock=json.loads(LOCK.read_bytes());old=a.parent_receipt.parent/"layout/blobs/sha256"
    require(receipt["source"]==PARENT and receipt["base_digest"]==lock["base_digest"] and receipt["offline_installs"][0]==receipt["offline_installs"][1],"Different dependency or base image")
    if a.fetch_locked:
        for p in lock["packages"]:
            f=cache/Path(p["path"]).name
            if not f.exists():
                require(p["path"].startswith("pool/main/") and ".." not in PurePosixPath(p["path"]).parts,"Unknown Debian path")
                with urllib.request.urlopen("https://deb.debian.org/debian/"+p["path"],timeout=45) as src:data=src.read(p["bytes"]+1)
                require(hashlib.sha256(data).hexdigest()==p["sha256"],"Changed Debian input")
                with f.open("xb") as dest:dest.write(data)
    target=a.output.resolve()/source;blobs=target/"layout/blobs/sha256";blobs.mkdir(parents=True,exist_ok=True)
    def read_blob(digest):
        path=old/digest[7:];require("sha256:"+sha(path)==digest,"Corrupt parent blob");return json.loads(path.read_bytes())
    def put(obj,kind):
        data=canonical(obj);h=hashlib.sha256(data).hexdigest();p=blobs/h
        if not p.exists():p.write_bytes(data)
        require(sha(p)==h,"Conflicting image metadata")
        return {"mediaType":kind,"digest":"sha256:"+h,"size":len(data)}
    parent=read_blob(receipt["image"]);cfg=read_blob(parent["config"]["digest"])
    require(parent["layers"][-1]["digest"]==receipt["offline_installs"][0]["layer"],"Parent dependency layer unbound")
    for d in parent["layers"]:
        src=old/d["digest"][7:];dst=blobs/src.name;require(sha(src)==src.name and src.stat().st_size==d["size"],"Parent layer integrity")
        if not dst.exists():
            try:os.link(src,dst)
            except OSError:shutil.copyfile(src,dst)
        require(sha(dst)==src.name,"Copied layer differs")
    runs=[]
    for _ in range(2):
        with tempfile.TemporaryDirectory(dir=target) as tmp:
            tmp=Path(tmp);out=tmp/"system.tar.gz"
            r=overlay(lock,cache,tmp,out);runs.append(r)
            dest=blobs/r["layer"][7:]
            if not dest.exists():os.link(out,dest)
    require(runs[0]==runs[1],"Independent system layers differ");r=runs[0]
    cfg["rootfs"]["diff_ids"].append(r["diff_id"]);cfg["config"]["Labels"]["org.opencontainers.image.revision"]=source
    cfg["history"].append({"created":"2000-01-01T00:00:00Z","created_by":"Orange verified Debian GL runtime data"})
    parent["config"]=put(cfg,"application/vnd.oci.image.config.v1+json")
    parent["layers"].append({"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":r["layer"],"size":r["bytes"]})
    image=put(parent,"application/vnd.oci.image.manifest.v1+json");tag="orange-dev-input/sdxl-cuda:"+source[:12]
    (target/"layout/oci-layout").write_bytes(canonical({"imageLayoutVersion":"1.0.0"}))
    (target/"layout/index.json").write_bytes(canonical({"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json",
        "manifests":[{**image,"platform":{"os":"linux","architecture":"amd64"},"annotations":{"org.opencontainers.image.ref.name":tag}}]}))
    final={**receipt,"source":source,"parent_image":receipt["image"],"dependency_source":PARENT,"system_installations":runs,"system_lock_sha256":sha(LOCK),
           "image":image["digest"],"config_digest":parent["config"]["digest"],"local_tag":tag}
    (target/"receipt.json").write_bytes(canonical(final)+b"\n");print(json.dumps(final),flush=True)

if __name__=="__main__":main()
