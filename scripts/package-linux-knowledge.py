#!/usr/bin/env python3.12
"""Package two matching Linux knowledge hosts and verify the extracted release in an isolated container."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

from linux_platform_inputs import local_image_id
from platform_payload import write_archive
from rust_notices import notices

ROOT = Path(__file__).resolve().parents[1]
PROGRAM = "orange-knowledge"
CHECKS = ("check-local-knowledge.py", "persistent_knowledge_acceptance.py")


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def source(commit, path):
    return subprocess.check_output(["git", "show", commit + ":" + path], cwd=ROOT)


def build_receipt(native, number, commit, lock):
    directory = native / str(number)
    record = json.loads((directory / "build.json").read_bytes())
    expected = {"source": commit, "target": lock["target"], "pass": number, "network": "none",
                "build_image": lock["build_image"]["image"], "protoc": lock["protoc"]["digest"],
                "binaries": {PROGRAM: digest(directory / PROGRAM)}}
    if any(record[key] != value for key, value in expected.items()):
        raise ValueError("Native build differs from its source, toolchain or executable receipt")
    return record


def smoke(archive, commit, docker, image, work):
    payload = work / "acceptance.tar"
    with tarfile.open(payload, "w") as bundle:
        bundle.add(archive, arcname="release.tar.gz", recursive=False)
        for name in CHECKS:
            data = source(commit, "scripts/" + name)
            entry = tarfile.TarInfo(name)
            entry.size = len(data)
            bundle.addfile(entry, io.BytesIO(data))
    command = ["set -eu", "umask 077", "cd /work", "tar -xf -",
               "tar -xzf release.tar.gz", "export TMPDIR=/work PYTHONDONTWRITEBYTECODE=1",
               "python3 check-local-knowledge.py --binary /work/orange-knowledge/bin/orange-knowledge"]
    with payload.open("rb") as stream:
        result = subprocess.check_output(docker + [
            "run", "--rm", "--pull=never", "--platform", "linux/arm64", "--network", "none",
            "--user", "65532:65532", "--read-only", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges", "--pids-limit", "128", "--memory", "512m",
            "--tmpfs", "/work:rw,exec,nosuid,nodev,uid=65532,gid=65532,mode=0700,size=160m",
            "--entrypoint", "/bin/sh", "-i", image, "-c", "\n".join(command)
        ], stdin=stream, text=True, timeout=180)
    print(result, end="", flush=True)
    records = [json.loads(line) for line in result.splitlines() if line.startswith("{")]
    [search] = [r for r in records if "top1_hits" in r]
    [saved] = [r for r in records if "persistent_restart_read" in r]
    if search["top1_hits"] != search["queries"] or search["queries"] < 8:
        raise ValueError("Linux live corpus qualification incomplete")
    if not all(saved[k] for k in ("persistent_restart_read", "unchanged_posting_segments_reused",
                                  "stale_cache_restore_rejected", "authority_loss_refused",
                                  "revocation_survived_restart", "interrupted_write_recovered")):
        raise ValueError("Linux persistent storage qualification incomplete")
    return {"live_search": search, "saved_index": saved}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True)
    parser.add_argument("--cache", type=Path, default=ROOT / ".local/linux")
    parser.add_argument("--context", default="colima-orange-agent")
    args = parser.parse_args()
    commit = args.source
    if len(commit) != 40 or any(c not in "0123456789abcdef" for c in commit):
        raise ValueError("Full source commit required")
    lock = json.loads(source(commit, "config/linux-platform-arm64.lock.json"))
    cargo = source(commit, "Cargo.lock")
    vendor = args.cache / ("vendor-" + hashlib.sha256(cargo).hexdigest())
    folder = ROOT / ".local/linux-knowledge-native" / commit
    builds = []
    for number in (1, 2):
        subprocess.run([sys.executable, ROOT / "scripts/build-linux-platform.py", "--profile", "knowledge",
                        "--source", commit, "--cache", args.cache.resolve(), "--context", args.context,
                        "--pass-number", str(number)], cwd=ROOT, check=True)
        builds.append(build_receipt(folder, number, commit, lock))
    if any(builds[0][key] != builds[1][key] for key in ("binaries", "context_sha256")):
        raise ValueError("Isolated Linux builds differ")
    docker = ["docker", "--context", args.context]
    image = local_image_id(docker, lock["build_image"])
    if not image:
        raise ValueError("Verified Linux acceptance image is missing")
    files = {"bin/" + PROGRAM: (folder / "1" / PROGRAM).read_bytes(), "Cargo.lock": cargo,
             "README.md": source(commit, "docs/local-selected-knowledge.md")}
    total, missing = notices(files, vendor.resolve(strict=True), cargo, lambda p: source(commit, p))
    if missing:
        raise ValueError("Missing locked source notice")
    manifest = {"source": commit, "target": lock["target"],
                "sha256": {p: hashlib.sha256(blob).hexdigest() for p, blob in sorted(files.items())}}
    files["manifest.json"] = (json.dumps(manifest, indent=2) + "\n").encode()
    dest = ROOT / ".local/knowledge-distributions"
    dest.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=dest) as path:
        tmp = Path(path)
        first, second = tmp / "first.tar.gz", tmp / "second.tar.gz"
        write_archive(first, files, prefix=PROGRAM)
        write_archive(second, files, prefix=PROGRAM)
        checksum = digest(first)
        if digest(second) != checksum:
            raise ValueError("Independent archive bytes differ")
        with tarfile.open(first) as archive:
            members = archive.getmembers()
            if len(members) != len(files):
                raise ValueError("Unexpected Linux archive inventory")
            for member in members:
                if not member.isfile() or not member.name.startswith(PROGRAM + "/"):
                    raise ValueError("Unexpected Linux archive member")
                key = member.name.removeprefix(PROGRAM + "/")
                if archive.extractfile(member).read() != files[key]:
                    raise ValueError("Linux archive content differs")
        verified = smoke(first, commit, docker, image, tmp)
        final = dest / (PROGRAM + "-linux-arm64-" + checksum[:16] + ".tar.gz")
        report = {"source": commit, "target": lock["target"], "archive": final.name, "sha256": checksum,
                  "bytes": first.stat().st_size, "independent_builds": builds, "identical_archives": 2,
                  "locked_crates": total, "without_packaged_notice": missing, "container_checks": verified}
        if final.exists():
            if final.is_symlink() or digest(final) != checksum:
                raise ValueError("Conflicting retained release")
        else:
            os.link(first, final)
        receipt = tmp / "receipt.json"
        receipt.write_text(json.dumps(report, indent=2) + "\n")
        os.replace(receipt, final.with_suffix(".json"))
        print(json.dumps(report), flush=True)


if __name__ == "__main__":
    main()
