#!/usr/bin/env python3
"""Prove a fixed historic export cannot alter the live checkout's same-version domain library."""
import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT=Path(__file__).resolve().parents[1]
cli=argparse.ArgumentParser()
cli.add_argument("--desktop-repo",type=Path,required=True)
args=cli.parse_args()
rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
with tempfile.TemporaryDirectory(prefix="orange-current-domain-") as folder:
    target=Path(folder)/"target"
    env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"],CARGO_NET_OFFLINE="true",
             CARGO_TARGET_DIR=str(target),CARGO_INCREMENTAL="0",CARGO_PROFILE_DEV_DEBUG="0")
    cmd=[cargo,"test","--locked","--offline","-p","orange-protocol","--features","grpc","--test","marketplace_examples"]
    def verify_current():
        result=subprocess.run(cmd,cwd=ROOT,env=env,check=True,text=True,stdout=subprocess.PIPE)
        if "four_typed_modalities_and_organized_retry_reconcile ... ok" not in result.stdout or "2 passed; 0 failed" not in result.stdout:
            raise ValueError("Required type references were not verified")
    def hashes():
        artifacts=sorted((target/"debug/deps").glob("liborange_domain-*.rlib"))
        if not artifacts:raise ValueError("Current compiled library absent")
        return {x.name:hashlib.sha256(x.read_bytes()).hexdigest() for x in artifacts}
    verify_current()
    before=hashes()
    subprocess.run([sys.executable,ROOT/"scripts/check-shared-runtime.py","--desktop-repo",args.desktop_repo.resolve()],
                   cwd=ROOT,env=env,check=True)
    if hashes()!=before:raise ValueError("Historical export replaced the current domain library")
    verify_current()
    print("PASS actual previous Runtime export, independent desktop import, identical current domain library and post-export type checks")
