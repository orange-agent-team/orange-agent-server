#!/usr/bin/env python3
"""Build one independent, network-isolated Linux arm64 platform binary set from an exact Git commit."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import uuid
import zipfile
from local_distribution_profiles import PLATFORM_PROGRAMS
from linux_platform_inputs import local_image_id

ROOT=Path(__file__).resolve().parents[1]
LOCK="config/linux-platform-arm64.lock.json"

def sha(path):
    h=hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda:stream.read(1024*1024),b""):h.update(block)
    return h.hexdigest()

def git(*args):
    return subprocess.check_output(["git",*args],cwd=ROOT)

def context_archive(commit,lock,cache,destination):
    vendor=cache/"vendor"
    if not vendor.is_dir():
        raise ValueError("Prepare the pinned crates with cargo vendor --locked --offline --versioned-dirs .local/linux/vendor")
    archive=cache/lock["protoc"]["name"]
    if archive.stat().st_size!=lock["protoc"]["size"] or "sha256:"+sha(archive)!=lock["protoc"]["digest"]:
        raise ValueError("Linux Protobuf package differs from producer release digest")
    def header(name,size,executable=False):
        t=tarfile.TarInfo(name);t.size=size;t.mode=0o755 if executable else 0o644;t.mtime=0
        return t
    with destination.open("xb") as output,gzip.GzipFile(fileobj=output,mode="wb",filename="",mtime=0,compresslevel=1) as zipout,tarfile.open(fileobj=zipout,mode="w|") as bundle:
        source=git("archive","--format=tar",commit)
        bundle.addfile(header("source.tar",len(source)),io.BytesIO(source))
        with zipfile.ZipFile(archive) as protoc:
            for item in sorted(protoc.infolist(),key=lambda member:member.filename):
                if item.is_dir() or (item.filename!="bin/protoc" and not item.filename.startswith("include/")):continue
                path=Path(item.filename)
                if path.is_absolute() or ".." in path.parts:raise ValueError("Invalid protocol compiler archive path")
                with protoc.open(item) as data:bundle.addfile(header("tool/"+item.filename,item.file_size,item.filename=="bin/protoc"),data)
        for f in sorted(vendor.rglob("*")):
            if f.is_symlink():raise ValueError("Unexpected symlink in vendored source")
            if f.is_file():
                with f.open("rb") as stream:
                    bundle.addfile(header("vendor/"+f.relative_to(vendor).as_posix(),f.stat().st_size,bool(f.stat().st_mode&0o111)),stream)

COMMAND="""set -euo pipefail
cd /tmp
mkdir -p orange-build/source
cd orange-build
tar -xzf -
tar -xf source.tar -C source
cd source
mkdir -p .cargo
printf '%s\\n' '[source.crates-io]' 'replace-with = "vendored"' '[source.vendored]' 'directory = "/tmp/orange-build/vendor"' > .cargo/config.toml
export PROTOC=/tmp/orange-build/tool/bin/protoc CARGO_HOME=/tmp/orange-build/cargo CARGO_TARGET_DIR=/tmp/orange-build/target
export RUSTFLAGS='--remap-path-prefix=/tmp/orange-build=/orange'
export CARGO_INCREMENTAL=0 CARGO_NET_OFFLINE=true CARGO_PROFILE_RELEASE_STRIP=symbols
rustc -V >&2
cargo -V >&2
"$PROTOC" --version >&2
test "$(rustc -V | cut -d' ' -f2)" = "$ORANGE_BUILD_RUST"
test "$("$PROTOC" --version)" = 'libprotoc 36.2'
args=()
for program in "$@"; do args+=(--bin "$program"); done
cargo build --locked --offline --release --target aarch64-unknown-linux-gnu -p orange-control-plane -p orange-worker "${args[@]}" >&2
cd "$CARGO_TARGET_DIR/aarch64-unknown-linux-gnu/release"
tar -cf - "$@"
"""

def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--pass-number",required=True,type=int,choices=(1,2))
    cli.add_argument("--context",default="colima-orange-agent")
    cli.add_argument("--cache",type=Path,default=ROOT/".local/linux")
    args=cli.parse_args();cache=args.cache.resolve()
    commit=git("rev-parse","HEAD").decode().strip()
    lock=json.loads(git("show",commit+":"+LOCK))
    stamp=git("show","-s","--format=%ct",commit).decode().strip()
    directory=ROOT/".local/linux-platform-native"/commit;directory.mkdir(parents=True,mode=0o700,exist_ok=True)
    target=directory/str(args.pass_number)
    if target.exists():
        receipt=json.loads((target/"build.json").read_bytes())
        if receipt["source"]!=commit or receipt["pass"]!=args.pass_number or receipt["build_image"]!=lock["build_image"]["image"] or set(receipt["binaries"])!=set(PLATFORM_PROGRAMS) or any(sha(target/k)!=v for k,v in receipt["binaries"].items()):
            raise ValueError("Retained native build no longer matches its complete receipt")
        print("Verified retained build",args.pass_number,commit,flush=True);return
    docker=["docker","--context",args.context]
    image=local_image_id(docker,lock["build_image"])
    if not image:raise ValueError("Prepare the pinned Linux build image")
    with tempfile.TemporaryDirectory(prefix=".native-",dir=directory) as tmp:
        work=Path(tmp);context=work/"context.tar.gz"
        context_archive(commit,lock,cache,context)
        outgoing=work/"binaries.tar";nonce=uuid.uuid4().hex;name="orange-platform-build-"+nonce
        command=docker+["run","--rm","--pull=never","--platform","linux/arm64","--name",name,"--label","orange.owned="+nonce,
            "--network","none","--cap-drop","ALL","--security-opt","no-new-privileges","--pids-limit","512",
            "--cpus","1.25","--memory","2600m","--user","65532:65532","--entrypoint","/bin/bash",
            "-e","CARGO_BUILD_JOBS=1","-e","RUSTUP_TOOLCHAIN="+lock["rust_version"],"-e","ORANGE_BUILD_RUST="+lock["rust_version"],
            "-e","SOURCE_DATE_EPOCH="+stamp,"-i",image,"-c",COMMAND,"build",*PLATFORM_PROGRAMS]
        print("Building Linux arm64 pass",args.pass_number,"source",commit,"isolated container",name,flush=True)
        try:
            with context.open("rb") as payload,outgoing.open("xb") as out:subprocess.run(command,stdin=payload,stdout=out,check=True,timeout=4000)
        finally:
            own=subprocess.run(docker+["inspect","--format",'{{index .Config.Labels "orange.owned"}}',name],capture_output=True,text=True)
            if own.returncode==0 and own.stdout.strip()==nonce:subprocess.run(docker+["rm","-f",name],check=True)
        ready=work/"output";ready.mkdir()
        with tarfile.open(outgoing) as bundle:
            entries=bundle.getmembers()
            if len(entries)!=len(PLATFORM_PROGRAMS) or {e.name for e in entries}!=set(PLATFORM_PROGRAMS):raise ValueError("Incomplete or duplicated binary set")
            for entry in entries:
                if not entry.isfile() or not 0<entry.size<=512*1024*1024:raise ValueError("Unexpected ELF member")
                path=ready/entry.name
                with bundle.extractfile(entry) as src,path.open("xb") as out:shutil.copyfileobj(src,out)
                with path.open("rb") as exe:hdr=exe.read(20)
                if hdr[:6]!=b"\x7fELF\x02\x01" or int.from_bytes(hdr[18:20],"little")!=183:raise ValueError("Expected aarch64 ELF: "+entry.name)
                path.chmod(0o755)
        receipt={"source":commit,"pass":args.pass_number,"target":lock["target"],"build_image":lock["build_image"]["image"],
                 "protoc":lock["protoc"]["digest"],"context_sha256":sha(context),"network":"none","binaries":{k:sha(ready/k) for k in PLATFORM_PROGRAMS}}
        (ready/"build.json").write_text(json.dumps(receipt,indent=2,sort_keys=True)+"\n")
        ready.rename(target);print(json.dumps(receipt,sort_keys=True),flush=True)

if __name__=="__main__":main()
