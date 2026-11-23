#!/usr/bin/env python3
"""Build, independently reproduce, unpack, verify and run the local macOS control-plane distribution."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
import tempfile

from local_distribution_profiles import PLATFORM_PROGRAMS
from platform_package_smoke import check

ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument("--database-url-file",type=Path,default=ROOT/".local/database-url")
parser.add_argument("--user-config",type=Path,default=ROOT/".local/identity/user-config.json")
parser.add_argument("--admin-config",type=Path,default=ROOT/".local/identity/admin-config.json")
parser.add_argument("--reuse-native",action="store_true",help="Use the separately verified two-build result for this same Git commit")
args=parser.parse_args()
if not __debug__ or sys.version_info[:2]!=(3,12) or (platform.system(),platform.machine())!=("Darwin","arm64"):
    raise SystemExit("Python 3.12 with assertions on macOS arm64 required")
git=lambda *args:subprocess.check_output(["git",*args],cwd=ROOT)
commit=git("rev-parse","HEAD").decode().strip()
native=ROOT/".local/platform-release"/commit
if not args.reuse_native:
    subprocess.run([sys.executable,ROOT/"scripts/build-embedding-release.py","--profile","platform","--commit",commit],cwd=ROOT,check=True)
report=json.loads((native/"native-build.json").read_bytes())
if (report["profile"]!="platform" or report["source_commit"]!=commit or report["target"]!="aarch64-apple-darwin" or not report["reproducible"]
    or len(report["builds"])!=2 or report["builds"][0]!=report["builds"][1] or set(report["builds"][0])!=set(PLATFORM_PROGRAMS)):
    raise ValueError("Two matching builds from this commit required")
sha=lambda data:hashlib.sha256(data).hexdigest()
files={}
for program in PLATFORM_PROGRAMS:
    data=(native/program).read_bytes()
    if sha(data)!=report["builds"][0][program]:raise ValueError("Binary digest mismatch")
    files["bin/"+program]=data
from platform_payload import service_files,write_archive
files.update(service_files(ROOT,commit,"distribution/platform/README.md"))
manifest={"source":commit,"target":report["target"],"build_macos":report["macos"],"programs":list(PLATFORM_PROGRAMS),
          "sha256":{n:sha(d) for n,d in sorted(files.items())}}
release=json.loads(files["config/worker-isolation-release.v1.json"])
manifest.update(wasm_enabled=release["wasm_enabled"],isolation_report_version=release["observation_version"],
                gpu_supply_from_diagnostic=release["supply_authorized_by_report"])
files["manifest.json"]=(json.dumps(manifest,indent=2)+"\n").encode()

dest=ROOT/".local/platform-distributions";dest.mkdir(mode=0o700,parents=True,exist_ok=True)
with tempfile.TemporaryDirectory(dir=dest,prefix=".verify-") as tmp:
    scratch=Path(tmp);first,second=scratch/"first.tgz",scratch/"second.tgz"
    write_archive(first,files);write_archive(second,files)
    with first.open("rb") as a,second.open("rb") as b:
        digest=hashlib.file_digest(a,"sha256").hexdigest()
        assert hashlib.file_digest(b,"sha256").hexdigest()==digest
    with tarfile.open(first) as tar:tar.extractall(scratch,filter="data")
    unpacked=scratch/"orange-platform"
    verified=json.loads(subprocess.check_output([sys.executable,"-I",unpacked/"verify.py"],text=True,cwd=scratch))
    smoke=check(unpacked,args.database_url_file,args.user_config.resolve(),args.admin_config.resolve(),scratch)
    from http_metrics_smoke import check_programs, ROLES
    smoke["private_http_metrics"]=check_programs(unpacked/"bin",args.database_url_file,args.user_config,args.admin_config,ROLES[1:])
    for program in (p for p in PLATFORM_PROGRAMS if p not in ("orange-user-api","orange-admin-api")):
        missing=subprocess.run([unpacked/"bin"/program],cwd=scratch,env={"PATH":"/usr/bin:/bin"},capture_output=True,timeout=12)
        if missing.returncode==0:raise ValueError("Unconfigured process started: "+program)
    target=dest/("orange-platform-macos-arm64-"+digest[:16]+".tar.gz")
    if target.exists():
        with target.open("rb") as f:assert hashlib.file_digest(f,"sha256").hexdigest()==digest
    else:os.link(first,target)
    record={"archive":target.name,"sha256":digest,"bytes":target.stat().st_size,"source":commit,"target":report["target"],
            "two_native_builds":report["builds"],"identical_archives":2,"verified_files":verified["verified_files"],"smoke":smoke,
            "limits":"Real loopback user/admin services; Worker and cloud binaries compiled and checked for refusal without configuration. No production deployment or signing."}
    target.with_suffix(".json").write_text(json.dumps(record,indent=2)+"\n")
    print(json.dumps(record))
