#!/usr/bin/env python3
"""Compile the locked native contracts and atomically publish verified descriptor bytes."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
lock = json.loads((root / "crates/protocol/generation.lock.json").read_bytes())
protoc = os.environ.get("PROTOC") or shutil.which("protoc") or "/opt/homebrew/bin/protoc"
if subprocess.check_output([protoc,"--version"],text=True).strip() != lock["compiler"]:
    raise SystemExit("Expected compiler " + lock["compiler"])
out = root / ".local/contracts"
out.mkdir(parents=True,exist_ok=True)
with tempfile.TemporaryDirectory(dir=out) as work:
    generated = Path(work) / "orange.pb"
    subprocess.run([protoc,"--proto_path=.","--descriptor_set_out="+str(generated),"--include_imports",
                    "worker.proto","host.proto"],check=True,cwd=root/"contracts/proto")
    digest = hashlib.sha256(generated.read_bytes()).hexdigest()
    if digest != lock["descriptor"]:
        raise SystemExit("Public descriptor differs from reviewed protocol lock")
    os.replace(generated,out/"orange.pb")
print("PASS locked Worker and cloud Host descriptor",digest)
