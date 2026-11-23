#!/usr/bin/env python3
"""Fetch the pinned llvm-mingw release on macOS into this checkout and verify it before extraction."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

ROOT=Path(__file__).resolve().parents[1]


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream,"sha256").hexdigest()


def main():
    lock=json.loads((ROOT/"config/windows-cross-toolchain.lock.json").read_bytes())
    dest=ROOT/".local/windows-cross"
    dest.mkdir(parents=True,exist_ok=True)
    tar=dest/lock["archive"]
    name=tar.name.removesuffix(".tar.xz")
    options=["curl","--fail","--silent","--show-error","--location","--proto","=https","--proto-redir","=https",
             "--connect-timeout","20","--retry","2","--retry-all-errors","--retry-max-time","360"]
    if not tar.exists():
        tag="https://api.github.com/repos/mstorsjo/llvm-mingw/releases/tags/"+lock["release"]
        release=json.loads(subprocess.check_output([*options,"--max-time","40",tag]))
        matches=[a for a in release["assets"] if a["name"]==lock["archive"] and a["size"]==lock["bytes"]
                 and a.get("digest")=="sha256:"+lock["sha256"] and a["browser_download_url"]==lock["url"]]
        if release["tag_name"]!=lock["release"] or len(matches)!=1:
            raise ValueError("Official asset metadata differs from the reviewed lock")
        asset="https://api.github.com/repos/mstorsjo/llvm-mingw/releases/assets/"+str(int(matches[0]["id"]))
        with tempfile.TemporaryDirectory(dir=dest) as folder:
            file=Path(folder)/"toolchain.tar.xz"
            subprocess.run([*options,"--max-time","360","-H","Accept: application/octet-stream","--output",file,asset],check=True)
            if file.stat().st_size!=lock["bytes"] or sha(file)!=lock["sha256"]:
                raise ValueError("Official archive bytes differ from the lock")
            os.link(file,tar)
    if tar.stat().st_size!=lock["bytes"] or sha(tar)!=lock["sha256"]:
        raise ValueError("Cached cross toolchain failed integrity verification")
    tree=dest/name
    if not tree.exists():
        with tempfile.TemporaryDirectory(dir=dest) as folder:
            with tarfile.open(tar,"r:xz") as source:
                source.extractall(folder,filter="data")
            src=Path(folder)/name
            if not (src/"bin/x86_64-w64-mingw32-clang").is_file():
                raise ValueError("Expected LLVM-MinGW x86_64 compiler")
            src.rename(tree)
    subprocess.run([tree/"bin/x86_64-w64-mingw32-clang","--version"],check=True)
    print("Verified pinned LLVM-MinGW toolchain",lock["release"],lock["sha256"])


if __name__=="__main__":
    main()
