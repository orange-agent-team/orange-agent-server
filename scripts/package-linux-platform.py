#!/usr/bin/env python3
"""Verify two clean ARM64 builds, run the extracted package on Ubuntu and publish a development archive."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
from rust_notices import notices
from linux_platform_smoke import smoke
from local_distribution_profiles import PLATFORM_PROGRAMS
from platform_payload import service_files,write_archive

ROOT=Path(__file__).resolve().parents[1]
LOCK="config/linux-platform-arm64.lock.json"

def git(*args):return subprocess.check_output(["git",*args],cwd=ROOT)
def sha(data):return hashlib.sha256(data).hexdigest()
def digest_file(path):
    with path.open("rb") as stream:return hashlib.file_digest(stream,"sha256").hexdigest()


def package(native,context,cache):
    packaging=git("rev-parse","HEAD").decode().strip()
    if not re.fullmatch("[0-9a-f]{40}",native):raise ValueError("Full native commit required")
    subprocess.run(["git","merge-base","--is-ancestor",native,packaging],cwd=ROOT,check=True)
    modified=git("diff","--name-only",native,packaging).decode().splitlines()
    if any(not s.startswith(("scripts/","distribution/","docs/")) for s in modified):
        raise ValueError("Native and packaging trees differ outside packaging tools or documentation; rebuild this commit")
    lock=json.loads(git("show",native+":"+LOCK))
    base=ROOT/".local/linux-platform-native"/native
    builds=[json.loads((base/str(n)/"build.json").read_bytes()) for n in (1,2)]
    for n,rec in enumerate(builds,1):
        if (rec["pass"]!=n or rec["source"]!=native or rec["network"]!="none" or rec["target"]!=lock["target"] or
            rec["build_image"]!=lock["build_image"]["image"] or rec["protoc"]!=lock["protoc"]["digest"] or set(rec["binaries"])!=set(PLATFORM_PROGRAMS)):
            raise ValueError("Independent locked native build receipt required")
    if builds[0]["binaries"]!=builds[1]["binaries"] or builds[0]["context_sha256"]!=builds[1]["context_sha256"]:
        raise ValueError("Independent build inputs or ELF outputs differ")
    files=service_files(ROOT,native,None,(LOCK,))
    files["README.md"]=git("show",packaging+":distribution/linux/README.md")
    for prog,expected in builds[0]["binaries"].items():
        if digest_file(base/"2"/prog)!=expected:raise ValueError("Second native output changed: "+prog)
        data=(base/"1"/prog).read_bytes()
        if sha(data)!=expected:raise ValueError("First native output changed: "+prog)
        files["bin/"+prog]=data
    vendor=cache/("vendor-"+sha(files["Cargo.lock"]))
    packages,missing=notices(files,vendor if vendor.is_dir() else cache/"vendor",files["Cargo.lock"],lambda path:git("show",packaging+":"+path))
    sums={name:sha(data) for name,data in sorted(files.items())}
    manifest={"source":native,"packaging_source":packaging,"target":lock["target"],"programs":list(PLATFORM_PROGRAMS),"sha256":sums}
    files["manifest.json"]=(json.dumps(manifest,indent=2,sort_keys=True)+"\n").encode()
    files["SHA256SUMS"]="".join(sha(data)+"  "+name+"\n" for name,data in sorted(files.items())).encode()
    dest=ROOT/".local/linux-platform-distributions";dest.mkdir(mode=0o700,parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(dir=dest,prefix=".verify-") as tmp:
        tmp=Path(tmp);first=tmp/"first.tgz";second=tmp/"second.tgz";write_archive(first,files);write_archive(second,files)
        dg=digest_file(first)
        if digest_file(second)!=dg:raise ValueError("Compressed package differs across writes")
        with tarfile.open(first) as tar:tar.extractall(tmp,filter="data")
        unpack=tmp/"orange-platform"
        verified=json.loads(subprocess.check_output([sys.executable,unpack/"verify.py"],text=True))
        confirmation=smoke(["docker","--context",context],unpack,lock)
        target=dest/f"orange-platform-linux-arm64-{dg[:16]}.tar.gz"
        try:os.link(first,target)
        except FileExistsError:
            if target.is_symlink() or digest_file(target)!=dg:raise ValueError("Existing content-addressed package changed")
        result={"archive":target.name,"sha256":dg,"bytes":target.stat().st_size,"native_source":native,"packaging_source":packaging,
                "target":lock["target"],"builds":builds,"identical_archives":2,"verified_files":verified["verified_files"],
                "locked_crates":packages,"without_packaged_notice":missing,"smoke":confirmation}
        receipt=dest/(target.name+"."+packaging[:12]+".json")
        raw=(json.dumps(result,indent=2,sort_keys=True)+"\n").encode()
        if receipt.exists() and receipt.read_bytes()!=raw:raise ValueError("Existing verification receipt changed")
        if not receipt.exists():receipt.write_bytes(raw)
        print(json.dumps(result,sort_keys=True),flush=True)

if __name__=="__main__":
    if not __debug__:raise SystemExit("Verification requires assertions")
    cli=argparse.ArgumentParser();cli.add_argument("--native-source",help="Defaults to the current Git commit")
    cli.add_argument("--context",default="colima-orange-agent");cli.add_argument("--cache",type=Path,default=ROOT/".local/linux")
    a=cli.parse_args();package(a.native_source or git("rev-parse","HEAD").decode().strip(),a.context,a.cache.resolve())
