#!/usr/bin/env python3
"""Verify an offline development artifact against an independently pinned Ed25519 release key."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile

PURPOSE = "orange.development-release.v1"
REPOSITORIES = {"orange-agent-server", "orange-agent-desktop", "orange-agent-web", "orange-agent-admin"}


def require(value, reason):
    if not value:
        raise ValueError(reason)


def regular(path):
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_BINARY", 0)
    stream = os.fdopen(os.open(path, flags), "rb")
    if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
        stream.close()
        raise ValueError("Regular verification input required")
    return stream


def bounded(path, limit):
    with regular(path) as stream:
        data = stream.read(limit + 1)
    require(len(data) <= limit, "Oversized verification input")
    return data


def archive_digest(path):
    with regular(path) as stream:
        before = os.fstat(stream.fileno())
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
        after = os.fstat(stream.fileno())
        require((before.st_size, before.st_mtime_ns, before.st_ctime_ns) ==
                (after.st_size, after.st_mtime_ns, after.st_ctime_ns), "Archive changed during verification")
        return digest


def unique_object(pairs):
    obj = {}
    for key, value in pairs:
        require(key not in obj, "Duplicate signed field")
        obj[key] = value
    return obj


def openssl():
    for path in ["/opt/homebrew/opt/openssl@3/bin/openssl", shutil.which("openssl")]:
        if path and Path(path).is_file():
            return path
    raise ValueError("OpenSSL with Ed25519 support required")


def verify(args):
    require(args.repository in REPOSITORIES and re.fullmatch(r"[a-z0-9][a-z0-9._-]{0,79}", args.target), "Invalid artifact scope")
    require(re.fullmatch(r"[a-f0-9]{40}", args.source) and re.fullmatch(r"[a-f0-9]{64}", args.key_sha256), "Invalid trusted pins")
    descriptor = bounded(args.descriptor, 4096)
    public = bounded(args.public_key, 2048)
    # SubjectPublicKeyInfo for Ed25519 has a fixed algorithm identifier and a 32-byte public key.
    match = re.fullmatch(rb"-----BEGIN PUBLIC KEY-----\r?\n([A-Za-z0-9+/=\r\n]+)-----END PUBLIC KEY-----(?:\r?\n)?", public)
    require(match is not None, "A single public release key is required")
    der = base64.b64decode(b"".join(match[1].splitlines()), validate=True)
    require(len(der) == 44 and der.startswith(bytes.fromhex("302a300506032b6570032100")), "Expected Ed25519 public key")
    require(hashlib.sha256(der).hexdigest() == args.key_sha256, "Public key differs from trusted release pin")
    signed = json.loads(descriptor.decode("utf-8"), object_pairs_hook=unique_object)
    digest = archive_digest(args.archive)
    expected = {"purpose":PURPOSE,"repository":args.repository,"source":args.source,"target":args.target,
                "key_spki_sha256":args.key_sha256,"archive_sha256":digest}
    require(signed == expected, "Signed descriptor differs from the requested release")
    signature = bounded(args.signature, 64)
    require(len(signature) == 64, "Invalid Ed25519 signature length")
    with tempfile.TemporaryDirectory(prefix="orange-release-check-") as folder:
        root = Path(folder)
        for filename, data in [("release.pub",public),("descriptor",descriptor),("signature",signature)]:
            (root / filename).write_bytes(data)
        result = subprocess.run([openssl(),"pkeyutl","-verify","-pubin","-inkey",root/"release.pub",
                                 "-rawin","-in",root/"descriptor","-sigfile",root/"signature"],
                                capture_output=True,timeout=10)
    require(result.returncode == 0, "Invalid development release signature")
    return expected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for field in ("archive","descriptor","signature","public-key"):
        parser.add_argument("--"+field, type=Path, required=True)
    for field in ("repository","source","target","key-sha256"):
        parser.add_argument("--"+field, required=True)
    args = parser.parse_args()
    try:
        print(json.dumps(verify(args), sort_keys=True))
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Verification failed: {error}\n")


if __name__ == "__main__":
    main()
