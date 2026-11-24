#!/usr/bin/env python3.12
"""Sign an already reviewed development model report with an independent Ed25519 key."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile


def unique(pairs):
    result={}
    for key,value in pairs:
        if key in result:raise ValueError("Duplicate JSON key")
        result[key]=value
    return result


def digest(report):
    data=json.dumps(report,sort_keys=True,ensure_ascii=False,separators=(",",":"),allow_nan=False).encode()
    return hashlib.sha256(b"orange-request-v1\0model-qualification-report.v1\0"+data).hexdigest()


def sign(report,notice,key):
    if report.get("purpose")!="model-qualification.v1" or report.get("environment")!="dev" or report.get("version")!=1:
        raise ValueError("Unknown signing purpose or environment")
    if hashlib.sha256(notice.encode()).hexdigest()!=report["license"]["notice_sha256"]:raise ValueError("License notice digest mismatch")
    if len(notice.encode())>131072 or not notice.strip():raise ValueError("Invalid license notice")
    key=Path(key)
    if not key.is_absolute() or key.is_symlink():raise ValueError("Absolute private signing key required")
    ssl="/opt/homebrew/opt/openssl@3/bin/openssl"
    if not Path(ssl).is_file():ssl=shutil.which("openssl")
    if not ssl:raise ValueError("OpenSSL is required")
    with os.fdopen(os.open(key,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK),"rb") as secret:
        meta=os.fstat(secret.fileno())
        if not stat.S_ISREG(meta.st_mode) or meta.st_uid!=os.getuid() or meta.st_nlink!=1 or meta.st_mode&0o077 or not 0<meta.st_size<=8192:
            raise ValueError("Unsafe private signing key")
        # Pin the validated open file across both operations. Only the public message goes on disk.
        fd=secret.fileno()
        private="/dev/fd/"+str(fd)
        public=subprocess.check_output([ssl,"pkey","-in",private,"-pubout","-outform","DER"],
                                       pass_fds=(fd,),stderr=subprocess.DEVNULL,timeout=15)
        if len(public)!=44 or public[:12]!=bytes.fromhex("302a300506032b6570032100"):
            raise ValueError("A dedicated Ed25519 key is required")
        secret.seek(0)
        with tempfile.TemporaryDirectory(prefix="orange-model-evidence-") as tmp:
            message=Path(tmp)/"message";message.write_text(digest(report))
            signature=subprocess.check_output([ssl,"pkeyutl","-sign","-inkey",private,"-rawin","-in",message],
                                              pass_fds=(fd,),stderr=subprocess.DEVNULL,timeout=15)
    return {"report":report,"license_notice":notice,"signing_key_sha256":hashlib.sha256(public[12:]).hexdigest(),
            "signature":base64.urlsafe_b64encode(signature).rstrip(b"=").decode()}


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--report",required=True,type=Path);cli.add_argument("--license-notice",required=True,type=Path)
    cli.add_argument("--signing-key",required=True,type=Path)
    args=cli.parse_args()
    if args.report.stat().st_size>65536 or args.license_notice.stat().st_size>131072:raise ValueError("Oversized review evidence")
    report=json.loads(args.report.read_bytes(),object_pairs_hook=unique)
    notice=args.license_notice.read_bytes().decode("utf-8")
    print(json.dumps(sign(report,notice,args.signing_key),ensure_ascii=False,separators=(",",":")))

if __name__=="__main__":main()
