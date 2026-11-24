#!/usr/bin/env python3.12
"""Qualify a digest-bound Linux platform archive using the real device, independent issuer and mTLS gateway."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

from linux_platform_smoke import smoke
from local_distribution_profiles import PLATFORM_PROGRAMS
from verified_platform_archive import unpack_archive

ROOT=Path(__file__).resolve().parents[1]
LOCK="config/linux-platform-arm64.lock.json"


def git(*args):
    return subprocess.check_output(["git",*args],cwd=ROOT)


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--receipt",type=Path,help="A prior release may be verified by a newer harness when its expected source and archive digest are both supplied")
    cli.add_argument("--native-source")
    cli.add_argument("--sha256")
    cli.add_argument("--context",default="colima-orange-agent")
    args=cli.parse_args()
    verifier=git("rev-parse","HEAD").decode().strip()
    if git("status","--porcelain"):raise ValueError("A clean committed verifier is required")
    if args.receipt is None:
        if args.native_source or args.sha256:raise ValueError("An explicit prior artifact requires its receipt")
        matches=[]
        for f in (ROOT/".local/linux-platform-distributions").glob("*.json"):
            record=json.loads(f.read_bytes())
            if record["native_source"]==verifier and record["packaging_source"]==verifier:matches.append(f)
        if len(matches)!=1:raise ValueError("Prepare both Linux passes and run scripts/package-linux-platform.py first")
        receipt=matches[0]
        source=verifier
    else:
        if not re.fullmatch(r"[0-9a-f]{40}",args.native_source or "") or not re.fullmatch(r"[0-9a-f]{64}",args.sha256 or ""):raise ValueError("Full expected native source and archive SHA-256 required")
        receipt=args.receipt.resolve(strict=True);source=args.native_source
    record=json.loads(receipt.read_bytes())
    if record["native_source"]!=source or record["packaging_source"]!=source or record["target"]!="aarch64-unknown-linux-gnu" or record["identical_archives"]!=2:
        raise ValueError("Wrong Linux source or target")
    digest=record["sha256"]
    if args.sha256 and digest!=args.sha256:raise ValueError("Unexpected archive digest")
    git("merge-base","--is-ancestor",source,verifier)
    lock_bytes=git("show",source+":"+LOCK);lock=json.loads(lock_bytes)
    first,second=record["builds"]
    if ([first["pass"],second["pass"]]!=[1,2] or first["binaries"]!=second["binaries"] or
        first["context_sha256"]!=second["context_sha256"] or set(first["binaries"])!=set(PLATFORM_PROGRAMS) or
        any(b["source"]!=source or b["network"]!="none" or b["build_image"]!=lock["build_image"]["image"] for b in (first,second))):
        raise ValueError("Independent native build evidence differs")
    archive=receipt.parent/record["archive"]
    if archive.parent!=receipt.parent or archive.is_symlink() or archive.stat().st_size!=record["bytes"]:raise ValueError("Invalid release path")
    with unpack_archive(archive,digest) as package:
        manifest=json.loads((package/"manifest.json").read_bytes())
        if manifest["source"]!=source or manifest["packaging_source"]!=source or (package/LOCK).read_bytes()!=lock_bytes:
            raise ValueError("Wrong extracted platform")
        for name,expected in first["binaries"].items():
            with (package/"bin"/name).open("rb") as stream:
                if hashlib.file_digest(stream,"sha256").hexdigest()!=expected:raise ValueError("ELF differs from clean builds")
        result=smoke(["docker","--context",args.context],package,lock,rotate_device=True)
    device=result["device_enrollment"]
    if not all(device.get(key) for key in ("stable_node_rotation","previous_control_denied","successor_withdrawn","duplicate_redemption_reused","independent_device_and_signer_filesystems")) or device["model_jobs"]!=0:
        raise ValueError("Incomplete Linux credential qualification")
    print(json.dumps({"verifier":verifier,"native":source,"archive":archive.name,"sha256":digest,"identical_archives":2,
                      "linux_arm64":result["linux_arm64"],"device_enrollment":device,"physical_gpu_verified":False}),flush=True)
    print("PASS Linux package rotated its device keys, retained its node, rejected old TLS and withdrew its successor",flush=True)


if __name__=="__main__":main()
