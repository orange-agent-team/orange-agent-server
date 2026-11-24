#!/usr/bin/env python3.12
"""Select and verify exact CPython 3.12 wheels for the Debian Bookworm AMD64 SDXL image."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time
import tomllib
import urllib.error
import urllib.request
from urllib.parse import unquote,urlsplit

try:
    from packaging.requirements import Requirement
    from packaging.tags import cpython_tags,compatible_tags
    from packaging.utils import parse_wheel_filename,canonicalize_name
except ImportError:
    from pip._vendor.packaging.requirements import Requirement
    from pip._vendor.packaging.tags import cpython_tags,compatible_tags
    from pip._vendor.packaging.utils import parse_wheel_filename,canonicalize_name

ROOT=Path(__file__).resolve().parents[1]
PROJECT=ROOT/"backends/environments/sdxl-linux-cuda"
BOOKWORM_GLIBC=36


def digest(path):
    with path.open("rb") as stream:return hashlib.file_digest(stream,"sha256").hexdigest()


def require(ok,reason):
    if not ok:raise ValueError(reason)


def selection():
    lock=tomllib.loads((PROJECT/"uv.lock").read_text())
    by_name={canonicalize_name(p["name"]):p for p in lock["package"]}
    exported=subprocess.check_output(["uv","export","--project",str(PROJECT),"--locked","--offline","--no-dev","--no-emit-project","--no-hashes","--no-header","--no-annotate"],text=True,
                                     env=dict(os.environ,UV_DEFAULT_INDEX="https://pypi.org/simple"),stderr=subprocess.PIPE)
    platforms=[]
    for minor in range(BOOKWORM_GLIBC,4,-1):
        platforms.append(f"manylinux_2_{minor}_x86_64")
        if minor in (5,12,17):platforms.append({5:"manylinux1",12:"manylinux2010",17:"manylinux2014"}[minor]+"_x86_64")
    platforms.append("linux_x86_64")
    order=list(cpython_tags((3,12),abis=["cp312"],platforms=platforms))+list(compatible_tags((3,12),"cp312",platforms))
    ranks={tag:i for i,tag in reversed(list(enumerate(order)))}
    env={"sys_platform":"linux","platform_machine":"x86_64","platform_system":"Linux","python_version":"3.12","python_full_version":"3.12.13",
         "implementation_name":"cpython","implementation_version":"3.12.13","platform_python_implementation":"CPython","os_name":"posix","extra":""}
    selected={}
    for line in exported.splitlines():
        if not line.strip() or line.startswith("#"):continue
        req=Requirement(line)
        if req.marker and not req.marker.evaluate(env):continue
        name=canonicalize_name(req.name);p=by_name[name]
        require(not req.url and str(req.specifier)=="=="+p["version"] and p["source"]=={"registry":"https://pypi.org/simple"},"Unpinned source: "+name)
        candidates=[]
        for w in p["wheels"]:
            url=urlsplit(w["url"]);filename=unquote(url.path.rsplit("/",1)[-1])
            require(url.scheme=="https" and url.hostname=="files.pythonhosted.org" and not url.query and Path(filename).name==filename,"Unknown wheel origin")
            distribution,version,_,tags=parse_wheel_filename(filename)
            valid=[ranks[t] for t in tags if t in ranks]
            if valid:
                require(distribution==name and str(version)==p["version"] and w["size"]>0 and w["hash"].startswith("sha256:"),"Wheel identity mismatch")
                candidates.append((min(valid),filename,w))
        require(candidates,"No Bookworm CPython 3.12 wheel for "+name)
        _,filename,entry=min(candidates,key=lambda v:(v[0],v[1]))
        selected[name]={"package":name,"version":p["version"],"filename":filename,"bytes":entry["size"],"sha256":entry["hash"][7:],"url":entry["url"]}
    require({"torch","torchvision","diffusers","safetensors","nvidia-cuda-runtime","nvidia-cudnn-cu13"}<=selected.keys(),"Incomplete CUDA inference environment")
    return {"schema":1,"python":"3.12.13","platform":"linux/amd64","glibc":"2.36","native_target":"cp312",
            "uv_lock_sha256":digest(PROJECT/"uv.lock"),"wheels":[selected[n] for n in sorted(selected)]}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self,*args):raise ValueError("Unexpected redirect from pinned PyPI blob")


def acquire(entry,cache,prior):
    target=cache/entry["filename"];partial=target.with_suffix(".whl.part");size=entry["bytes"]
    def matches(path):return path.is_file() and not path.is_symlink() and path.stat().st_size==size and digest(path)==entry["sha256"]
    if target.exists():
        require(matches(target),"Existing wheel differs from lock: "+target.name);return
    for previous in prior:
        src=previous/target.name
        if matches(src):
            shutil.copyfile(src,partial)
            require(matches(partial),"Reused wheel copy changed")
            os.replace(partial,target);return
    opener=urllib.request.build_opener(NoRedirect())
    for attempt in range(3):
        try:
            offset=partial.stat().st_size if partial.exists() else 0
            require(not partial.is_symlink(),"Unsafe partial path")
            if offset>size:raise ValueError("Partial file exceeds its bound")
            if offset<size:
                headers={"Accept-Encoding":"identity"}
                if offset:headers["Range"]=f"bytes={offset}-"
                with opener.open(urllib.request.Request(entry["url"],headers=headers),timeout=35) as response:
                    if response.status==206:
                        require(response.headers.get("Content-Range")==f"bytes {offset}-{size-1}/{size}","Invalid wheel range")
                    elif response.status==200:offset=0
                    else:raise ValueError("Unexpected wheel response")
                    with partial.open("ab" if offset else "wb") as out:
                        while block:=response.read(1024*1024):
                            require(out.tell()+len(block)<=size,"Oversized wheel")
                            out.write(block)
                        out.flush();os.fsync(out.fileno())
            if matches(partial):
                os.replace(partial,target);print("Verified",target.name,flush=True);return
            if partial.stat().st_size==size:partial.unlink()
            raise OSError("Incomplete wheel")
        except (OSError,urllib.error.URLError) as err:
            if attempt==2:raise
            time.sleep(2**attempt)
    raise RuntimeError("Unreachable wheel retry")


def main():
    cli=argparse.ArgumentParser()
    mode=cli.add_mutually_exclusive_group(required=True)
    mode.add_argument("--seal-platform-lock",action="store_true");mode.add_argument("--verify-lock",action="store_true")
    mode.add_argument("--download",action="store_true");mode.add_argument("--verify-cache",action="store_true")
    cli.add_argument("--cache",type=Path,default=ROOT/".local/linux-sdxl-wheelhouse")
    cli.add_argument("--reuse",type=Path,action="append",default=[])
    args=cli.parse_args();lock_path=PROJECT/"wheels.lock.json"
    actual=selection()
    if args.seal_platform_lock:lock_path.write_text(json.dumps(actual,indent=2)+"\n")
    require(json.loads(lock_path.read_bytes())==actual,"Target wheel selection differs from the reviewed lock")
    wheels=actual["wheels"]
    if args.download:
        import fcntl
        args.cache.mkdir(parents=True,exist_ok=True,mode=0o700)
        with (args.cache/".prepare.lock").open("a") as guard:
            fcntl.flock(guard,fcntl.LOCK_EX)
            with ThreadPoolExecutor(max_workers=3) as pool:
                list(pool.map(lambda e:acquire(e,args.cache,args.reuse),wheels))
    if args.download or args.verify_cache:
        for e in wheels:
            p=args.cache/e["filename"];require(p.is_file() and not p.is_symlink() and p.stat().st_size==e["bytes"] and digest(p)==e["sha256"],"Missing or changed cached wheel: "+p.name)
    print(json.dumps({"target":actual["platform"],"python":actual["python"],"packages":len(wheels),"bytes":sum(e["bytes"] for e in wheels),
                      "cache_verified":bool(args.download or args.verify_cache),"platform_lock_sha256":digest(lock_path)}),flush=True)


if __name__=="__main__":main()
