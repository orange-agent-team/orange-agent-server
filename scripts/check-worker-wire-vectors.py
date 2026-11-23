#!/usr/bin/env python3
"""Generate and consume v1 wire fixtures using the original shipped control schema and the current one."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
FLOOR = ROOT / "contracts/fixtures/worker-v1-baseline"
VECTORS = json.loads((FLOOR / "vectors.json").read_bytes())
SCHEMAS = {"floor": FLOOR, "current": ROOT/"contracts/proto"}
PROTOC = os.environ.get("PROTOC") or shutil.which("protoc") or "/opt/homebrew/bin/protoc"


def transcode(schema, mode, message, data):
    return subprocess.check_output([PROTOC, "--proto_path=.", f"--{mode}=orange.worker.v1.{message}", "worker.proto"],
                                   cwd=SCHEMAS[schema], input=data)


def main():
    lock = json.loads((ROOT/"crates/protocol/generation.lock.json").read_bytes())
    if subprocess.check_output([PROTOC, "--version"], text=True).strip() != lock["compiler"]:
        raise ValueError("Expected the pinned Protobuf compiler")
    floor = (FLOOR/"worker.proto").read_bytes()
    if hashlib.sha256(floor).hexdigest() != VECTORS["schema_sha256"]:
        raise ValueError("Historical v1 source changed")
    seen = {}
    for case in VECTORS["samples"]:
        name, kind = case["name"], case["message"]
        if name in seen:
            raise ValueError("Duplicate fixture")
        wire = transcode(case["producer"],"encode",kind,case["textproto"].encode())
        if wire.hex() != case["hex"]:
            raise ValueError(f"Wire bytes changed for {name}")
        seen[name] = {}
        for reader in SCHEMAS:
            decoded = transcode(reader,"decode",kind,wire).decode()
            # protoc exposes unrecognized fields as numeric tags. Preserve only the
            # named fields this specific reader can interpret when testing its view.
            known = "\n".join(line for line in decoded.splitlines() if not line.lstrip()[:1].isdigit())
            seen[name][reader] = transcode(reader,"encode",kind,known.encode())
    for message in ("hello","welcome"):
        old, new = seen["floor-"+message], seen["current-"+message]
        if old["floor"] != old["current"] or new["floor"] != old["floor"] or new["current"] == old["current"]:
            raise ValueError(f"Additive {message} fields affected the legacy peer or were lost by the current peer")
    print("PASS pinned historical and current Protobuf peer projections and exact wire bytes")


if __name__ == "__main__":
    main()
