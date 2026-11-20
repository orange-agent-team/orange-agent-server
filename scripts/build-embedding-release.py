#!/usr/bin/env python3
"""Build MP-03 release executables from two independent pinned source trees."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--commit", required=True)
args = parser.parse_args()
if len(args.commit)!=40 or any(c not in "0123456789abcdef" for c in args.commit):
    raise SystemExit("Supply a complete immutable local commit")
if platform.system()!="Darwin" or platform.machine()!="arm64":
    raise SystemExit("Only the macOS arm64 development distribution is qualified here")
dest = root / ".local/embedding-release" / args.commit
dest.mkdir(parents=True,exist_ok=True)
target = "aarch64-apple-darwin"
rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo = Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
epoch = subprocess.check_output(["git","show","-s","--format=%ct",args.commit],cwd=root,text=True).strip()
report = {"source_commit":args.commit,"target":target,"macos":platform.mac_ver()[0],"rustc":subprocess.check_output([cargo.parent/"rustc","--version"],text=True).strip()}
builds=[]
with tempfile.TemporaryDirectory(prefix="oe-build-") as tmp:
    tmp=Path(tmp)
    archive=tmp/"source.tar"
    with archive.open("wb") as out:
        subprocess.run(["git","archive",args.commit],cwd=root,stdout=out,check=True)
    for i in range(2):
        tree=tmp/str(i)/"source"
        tree.mkdir(parents=True)
        with tarfile.open(archive) as src:
            for entry in src:
                if not (entry.isdir() or entry.isfile()) or not (tree/entry.name).resolve().is_relative_to(tree.resolve()):
                    raise ValueError("unexpected tracked archive entry")
                src.extract(entry,tree)
        env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ['PATH'],CARGO_INCREMENTAL="0",CARGO_NET_OFFLINE="true",
                 CARGO_TARGET_DIR=str(tree/'target'),CARGO_PROFILE_RELEASE_STRIP="symbols",SOURCE_DATE_EPOCH=epoch,
                 RUSTFLAGS="--remap-path-prefix="+str(tree)+"=/orange-agent",CFLAGS="-ffile-prefix-map="+str(tree)+"=/orange-agent")
        subprocess.run([cargo,"build","--locked","--offline","--target",target,"--release","-p","orange-model-runtime",
                        "--features","embedding-host","--bin","orange-embedding-host","--example","local-embeddings"],cwd=tree,env=env,check=True)
        files={}
        for name in ("orange-embedding-host","examples/local-embeddings"):
            artifact=tree/"target"/target/"release"/name
            linkage=subprocess.check_output(["otool","-L",artifact],text=True).splitlines()[1:]
            if any(not line.strip().startswith(("/usr/lib/","/System/Library/")) for line in linkage):
                raise ValueError("unbundled native dependency")
            files[artifact.name]=hashlib.sha256(artifact.read_bytes()).hexdigest()
            if i==1 and builds[0][artifact.name]==files[artifact.name]:
                shutil.copy2(artifact,dest/artifact.name)
        builds.append(files)
    report.update(builds=builds,reproducible=(builds[0]==builds[1]))
    (dest/"native-build.json").write_text(json.dumps(report,indent=2)+"\n")
    if not report["reproducible"]:
        raise SystemExit("Native binaries differ; see native-build.json")
    print(json.dumps(report))
