#!/usr/bin/env python3
"""Exercise the delivered verifier with independent ephemeral release and device keys."""
import argparse
from contextlib import ExitStack
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--archive",type=Path,help="Use a real local development bundle as the immutable byte input")
    parser.add_argument("--installed",action="store_true",help="Exercise the verifier shipped in the reproducible package from this commit")
    parser.add_argument("--source",help="Expected Git source of the test archive")
    args=parser.parse_args()
    source=args.source or subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    crypto="/opt/homebrew/opt/openssl@3/bin/openssl"
    if not Path(crypto).is_file():crypto=shutil.which("openssl")
    if not crypto:raise RuntimeError("OpenSSL required")
    with ExitStack() as stack:
        folder=Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="orange-release-purpose-")))
        verifier=ROOT/"distribution/platform/verify-candidate-signature.py"
        if args.installed:
            if args.archive or args.source:parser.error("--installed selects its own exact archive and source")
            from verified_platform_archive import unpack_archive
            dist=ROOT/".local/platform-distributions"
            records=[json.loads(path.read_bytes()) for path in dist.glob("*.tar.json")]
            record,=[r for r in records if r["source"]==source and r["identical_archives"]==2]
            candidate=dist/record["archive"]
            if candidate.parent!=dist or candidate.is_symlink():raise ValueError("Unexpected release path")
            package=stack.enter_context(unpack_archive(candidate,record["sha256"]))
            args.archive=candidate
            verifier=package/"verify-candidate-signature.py"
        def command(*options):
            return subprocess.check_output([crypto,*options],cwd=folder,stderr=subprocess.PIPE)
        for name in ("release","device"):
            command("genpkey","-algorithm","Ed25519","-out",name+".key")
            command("pkey","-in",name+".key","-pubout","-out",name+".pub")
        pin=hashlib.sha256(command("pkey","-pubin","-in","release.pub","-outform","DER")).hexdigest()
        archive=args.archive.resolve(strict=True) if args.archive else folder/"candidate"
        if not args.archive:archive.write_bytes(b"development verifier input")
        with archive.open("rb") as stream:digest=hashlib.file_digest(stream,"sha256").hexdigest()
        descriptor={"purpose":"orange.development-release.v1","repository":"orange-agent-server","source":source,
                    "target":"aarch64-apple-darwin","key_spki_sha256":pin,"archive_sha256":digest}
        def sign(payload, key="release"):
            (folder/"descriptor").write_bytes(payload)
            command("pkeyutl","-sign","-inkey",key+".key","-rawin","-in","descriptor","-out","signature")
        def check(expected, **changes):
            options=dict(archive=archive,descriptor="descriptor",signature="signature",public_key="release.pub",
                         repository=descriptor["repository"],source=source,target=descriptor["target"],key_sha256=pin)
            options.update(changes)
            cmd=[sys.executable,"-I",verifier.resolve()]
            for flag,value in options.items():cmd.extend(["--"+flag.replace("_","-"),str(value)])
            result=subprocess.run(cmd,cwd=folder,capture_output=True,timeout=12)
            if expected:
                if result.returncode or json.loads(result.stdout)!=descriptor:raise ValueError("Genuine test release refused")
            elif result.returncode==0 or result.stdout:raise ValueError("Untrusted release accepted")
        original=json.dumps(descriptor,sort_keys=True).encode()
        sign(original)
        check(True)
        check(False,repository="orange-agent-admin")
        check(False,source=("b" if source[0]=="a" else "a")+source[1:])
        check(False,target="x86_64-pc-windows-gnullvm")
        check(False,public_key="device.pub")
        check(False,public_key="release.key")
        (folder/"other").write_bytes(b"different archive")
        check(False,archive="other")
        sign(original,key="device")
        check(False)
        sign(json.dumps(dict(descriptor,purpose="orange.artifact-redactions.v1")).encode())
        check(False)
        sign(b'{"purpose":"other",'+original[1:])
        check(False)
        sign(original)
        (folder/"signature").write_bytes(b"")
        check(False)
        print(json.dumps({"archive_sha256":digest,"source":source,"ephemeral_authority":True,
                          "real_archive":bool(args.archive),"installed_verifier":args.installed,"positive":1,"denials":10}),flush=True)
    print("PASS independent purpose, repository, target, source, archive and release signature verification")


if __name__=="__main__":
    main()
