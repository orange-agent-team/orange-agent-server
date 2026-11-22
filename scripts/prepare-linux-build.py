#!/usr/bin/env python3
"""Prepare exact Linux build inputs. Fetching public inputs is explicit; no Docker host mounts are needed."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.request
from linux_platform_inputs import local_image_id

ROOT=Path(__file__).resolve().parents[1]

def file_sha(path):
    with path.open("rb") as stream:return hashlib.file_digest(stream,"sha256").hexdigest()

def verify_vendor(directory,locked):
    seen=set()
    if not directory.is_dir():return False
    for package in directory.iterdir():
        if not package.is_dir() or package.is_symlink():return False
        try:
            meta=tomllib.loads((package/"Cargo.toml").read_text())["package"]
            checks=json.loads((package/".cargo-checksum.json").read_bytes())
        except (OSError,KeyError,ValueError):return False
        key=meta["name"],meta["version"]
        if key in seen or locked.get(key)!=checks["package"]:return False
        seen.add(key)
    return seen==set(locked)

def main():
    cli=argparse.ArgumentParser();cli.add_argument("--fetch",action="store_true")
    cli.add_argument("--cache",type=Path,default=ROOT/".local/linux");cli.add_argument("--context",default="colima-orange-agent")
    a=cli.parse_args();cache=a.cache.resolve();cache.mkdir(parents=True,mode=0o700,exist_ok=True)
    lock=json.loads((ROOT/"config/linux-platform-arm64.lock.json").read_bytes())
    zipfile=cache/lock["protoc"]["name"]
    def valid_zip(path):
        return path.is_file() and not path.is_symlink() and path.stat().st_size==lock["protoc"]["size"] and "sha256:"+file_sha(path)==lock["protoc"]["digest"]
    if not valid_zip(zipfile):
        if not a.fetch:raise SystemExit("Pinned Protobuf missing; prepare with --fetch")
        for attempt in range(3):
            with tempfile.NamedTemporaryFile(dir=cache,delete=False) as out:
                staged=Path(out.name)
                try:
                    with urllib.request.urlopen(lock["protoc"]["browser_download_url"],timeout=60) as response:
                        data=response.read(lock["protoc"]["size"]+1)
                    out.write(data);out.flush()
                    if not valid_zip(staged):raise ValueError("Protobuf digest mismatch")
                    os.replace(staged,zipfile);break
                except (OSError,ValueError):
                    if attempt==2:raise
                finally:staged.unlink(missing_ok=True)
            time.sleep(1<<attempt)
    cargo_lock=(ROOT/"Cargo.lock").read_bytes()
    locked={(r["name"],r["version"]):r["checksum"] for r in tomllib.loads(cargo_lock.decode())["package"] if r.get("source","").startswith("registry+")}
    versioned=cache/("vendor-"+hashlib.sha256(cargo_lock).hexdigest())
    if not any(verify_vendor(folder,locked) for folder in (versioned,cache/"vendor")):
        if versioned.exists():raise ValueError("Existing version-specific vendor directory differs from its lock")
        with tempfile.TemporaryDirectory(dir=cache,prefix="crate-inputs-") as tmp:
            path=Path(tmp)/"vendor";tool=shutil.which("cargo") or "/opt/homebrew/opt/rustup/bin/cargo"
            subprocess.run([tool,"vendor","--locked",*(() if a.fetch else ("--offline",)),"--versioned-dirs",str(path)],cwd=ROOT,check=True,stdout=subprocess.DEVNULL)
            if not verify_vendor(path,locked):raise ValueError("Vendored crates differ from Cargo.lock")
            path.rename(versioned)
    if a.fetch:
        subprocess.run([sys.executable,ROOT/"scripts/prepare-linux-images.py","--cache",cache/"oci","--context",a.context,
                        "--image","build_image","--image","runtime_image","--image","test_database"],check=True)
    for entry in ("build_image","runtime_image","test_database"):
        if not local_image_id(["docker","--context",a.context],lock[entry]):raise ValueError("Missing locked local image: "+entry)
    print("Verified Protobuf and",len(locked),"Cargo source packages plus pinned Linux images")

if __name__=="__main__":main()
