#!/usr/bin/env python3.12
"""Rebuild the qualified offline development image from its two immutable Git revisions."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PYTHON_SOURCE = "cdad5d375fd394251a29a062e0311c2696de917c"
IMAGE_SOURCE = "f8882a6d2d8b4f94f6e64fff092e85aacd1894c8"
IMAGE = "sha256:b01f7baed816b97294e079f8a77493e0049d70c8c553cffff183b696a642fd33"


def run(*cmd, cwd):
    subprocess.run(cmd, cwd=cwd, check=True)


def checkout(root, name, commit):
    target = root / name
    run("git", "-c", "protocol.file.allow=always", "clone", "--local", "--quiet", "--no-checkout", "--no-tags",
        ROOT, target, cwd=root)
    run("git", "-c", "advice.detachedHead=false", "checkout", "--quiet", "--detach", commit, cwd=target)
    if subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=target, text=True).strip() != commit:
        raise RuntimeError("Unexpected source checkout")
    return target


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cache", type=Path, required=True, help="Private directory for base-oci, wheels, debian and outputs")
    parser.add_argument("--expected-sha256",help="Check the final tar against the independent release receipt")
    parser.add_argument("--context", default="default")
    parser.add_argument("--fetch", action="store_true", help="Fetch verified missing public inputs before the offline installations")
    parser.add_argument("--verify-inputs", action="store_true", help="Only check that both complete source snapshots are available")
    args = parser.parse_args()
    cache = args.cache.resolve()
    cache.mkdir(mode=0o700, exist_ok=True, parents=True)
    runner_source=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    if subprocess.check_output(["git","status","--porcelain"],cwd=ROOT):
        raise RuntimeError("Build from a clean tagged or detached checkout")
    with tempfile.TemporaryDirectory(prefix="orange-sdxl-build-") as tmp:
        py = checkout(Path(tmp), "python", PYTHON_SOURCE)
        sysroot = checkout(Path(tmp), "debian", IMAGE_SOURCE)
        if args.verify_inputs:
            for tree, folder in [(py, "wheels.lock.json"), (sysroot, "debian-libs.lock.json")]:
                data = tree / "backends/environments/sdxl-linux-cuda" / folder
                print(folder, hashlib.sha256(data.read_bytes()).hexdigest())
            print("runner.py",hashlib.sha256((ROOT/"distribution/sdxl-linux/runner.py").read_bytes()).hexdigest())
            return
        wheels, base, out = (cache / name for name in ("wheels", "base-oci", "outputs"))
        if args.fetch:
            run(sys.executable, "scripts/prepare-linux-sdxl-wheels.py", "--download", "--cache", wheels, cwd=py)
            run(sys.executable, "scripts/prepare-linux-images.py", "--image", "sdxl_python", "--cache", base, "--context", args.context, cwd=py)
        run(sys.executable, "scripts/prepare-linux-sdxl-wheels.py", "--verify-cache", "--cache", wheels, cwd=py)
        run(sys.executable, "scripts/build-linux-sdxl-image.py", "--wheel-cache", wheels, "--base-cache", base,
            "--context", args.context, "--output", out, cwd=py)
        run(sys.executable, "scripts/assemble-sdxl-system-layer.py", "--parent-receipt", out / PYTHON_SOURCE / "receipt.json",
            "--packages", cache / "debian", "--output", out, *(["--fetch-locked"] if args.fetch else []), cwd=sysroot)
        final = out / IMAGE_SOURCE
        if json.loads((final / "receipt.json").read_bytes())["image"] != IMAGE:
            raise RuntimeError("Rebuilt OCI manifest differs from the qualified image")
        run(sys.executable, "scripts/assemble-sdxl-runner-layer.py", "--base", final, "--output", out, cwd=ROOT)
        final = out / runner_source
        run(sys.executable, "scripts/check-linux-sdxl-image.py", "--receipt", final / "receipt.json", "--context", args.context, cwd=ROOT)
        proof = json.loads((final / "qualification.json").read_bytes())
        if (args.expected_sha256 and proof["sha256"] != args.expected_sha256) or proof["hardware_certified"] or not proof["no_device_denied"]:
            raise RuntimeError("Archive or local qualification differs")
        print(final / proof["archive"])


if __name__ == "__main__":
    main()
