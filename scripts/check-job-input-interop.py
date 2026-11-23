#!/usr/bin/env python3
"""Round-trip fixed marketplace references through the real Rust and Dart codecs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = "contracts/fixtures/job-input-interop.v1.json"


def exchange(command, inputs, cwd, env):
    data = subprocess.check_output(command, cwd=cwd, env=env, input=json.dumps(inputs).encode(), timeout=120)
    return json.loads(data)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--desktop-repo", type=Path, required=True)
    parser.add_argument("--dart", type=Path)
    args = parser.parse_args()
    desktop = args.desktop_repo.resolve()
    dart = (args.dart or (desktop / ".local/toolchains/flutter-3.47.9/flutter/bin/cache/dart-sdk/bin/dart")).resolve()
    source = (ROOT / FIXTURE).read_bytes()
    lock = json.loads((desktop / "config/job-input-contract.lock.json").read_bytes())
    if lock["fixture"] != FIXTURE or len(lock["source_commit"]) != 40:
        raise ValueError("Invalid desktop contract source")
    frozen = subprocess.check_output(["git", "show", lock["source_commit"] + ":" + FIXTURE], cwd=ROOT)
    peer = (desktop / FIXTURE).read_bytes()
    if frozen != source or peer != source or hashlib.sha256(source).hexdigest() != lock["sha256"]:
        raise ValueError("Desktop snapshot and pinned server fixture differ")
    cases = json.loads(source)
    inputs = [case["wire"] for case in cases]
    expected = [case["wire"] if case["accepted"] else None for case in cases]
    positives = [case["wire"] for case in cases if case["accepted"]]
    rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo = Path(subprocess.check_output([rustup, "which", "--toolchain", "1.97.1", "cargo"], text=True).strip())
    env = dict(os.environ, PATH=str(cargo.parent) + os.pathsep + os.environ["PATH"], CARGO_NET_OFFLINE="true")
    subprocess.run([cargo, "build", "--locked", "--offline", "-p", "orange-protocol", "--example", "job_input_codec"],
                   cwd=ROOT, env=env, check=True)
    meta = json.loads(subprocess.check_output([cargo, "metadata", "--format-version", "1", "--no-deps",
                                               "--offline", "--locked"], cwd=ROOT, env=env))
    binary = Path(meta["target_directory"]) / "debug/examples/job_input_codec"
    dart_cmd = [dart, desktop / "apps/desktop/tool/job_input_codec.dart"]
    rust = exchange([binary], inputs, ROOT, env)
    dart_values = exchange(dart_cmd, inputs, desktop, env)
    for peer_name, values in [("Rust", rust), ("Dart", dart_values)]:
        if values != expected:
            raise ValueError(f"{peer_name} acceptance or re-encoding differed: "
                             f"{[case['name'] for case, want, got in zip(cases, expected, values) if want != got]}")
    for cmd, cwd, values in [([binary], ROOT, dart_values), (dart_cmd, desktop, rust)]:
        if exchange(cmd, [v for v in values if v is not None], cwd, env) != positives:
            raise ValueError("Cross-language re-encoding differed")
    print(f"PASS Rust and Dart four-modality round trips; {len(positives)} positive and "
          f"{len(cases)-len(positives)} invalid fixture values; source {lock['source_commit']}")


if __name__ == "__main__":
    main()
