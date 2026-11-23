#!/usr/bin/env python3
"""Check a released Linux device-enrollment receipt, or exercise one native pass while a second builds."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from linux_platform_smoke import smoke
from local_distribution_profiles import PLATFORM_PROGRAMS
from platform_payload import service_files

ROOT=Path(__file__).resolve().parents[1]
NATIVE_LOCK="config/linux-platform-arm64.lock.json"


def require(ok,message):
    if not ok:raise ValueError(message)


def digest(path):
    with path.open("rb") as stream:return hashlib.file_digest(stream,"sha256").hexdigest()


def git(*args):
    return subprocess.check_output(["git",*args],cwd=ROOT)


def validate_smoke(result):
    expected={"real_handoff":True,"prepared_certificates":1,"real_redemption":True,"repaired_pem_without_token":True,
        "verified_grpc_welcome":True,"revoked_reconnect_denied":True,"independent_device_and_signer_filesystems":True,"model_jobs":0}
    require(result.get("device_enrollment")==expected and result["linux_arm64"] and result["public_ports"]==0 and
            result["independent_oidc_clients"] and result["actual_user_api"] and result["actual_admin_api"] and result["all_six_executables_loaded"],"Missing installed Linux service checks")


def main():
    p=argparse.ArgumentParser()
    p.add_argument("--native-source",help="Defaults to the current commit")
    p.add_argument("--single-pass",action="store_true",help="Run the real smoke on ELF bytes of build pass 1 without publishing an archive")
    p.add_argument("--context",default="colima-orange-agent")
    a=p.parse_args();head=git("rev-parse","HEAD").decode().strip();native=a.native_source or head
    require(len(native)==40 and all(c in "0123456789abcdef" for c in native) and not git("status","--porcelain"),"Clean exact native source and verifier required")
    subprocess.run(["git","merge-base","--is-ancestor",native,head],cwd=ROOT,check=True)
    require(all(n.startswith(("scripts/","docs/","distribution/")) for n in git("diff","--name-only",native,head).decode().splitlines()),"Native source changed")
    lock=json.loads(git("show",native+":"+NATIVE_LOCK))
    base=ROOT/".local/linux-platform-native"/native
    if a.single_pass:
        one=json.loads((base/"1/build.json").read_bytes())
        require(one["pass"]==1 and one["source"]==native and one["network"]=="none" and one["target"]==lock["target"] and one["build_image"]==lock["build_image"]["image"]
              and set(one["binaries"])==set(PLATFORM_PROGRAMS),"Invalid first native build receipt")
        with tempfile.TemporaryDirectory() as folder:
            package=Path(folder)/"orange-platform";package.mkdir()
            files=service_files(ROOT,native,None,(NATIVE_LOCK,))
            files["README.md"]=git("show",head+":distribution/linux/README.md")
            for name,expected in one["binaries"].items():
                binary=base/"1"/name
                require(digest(binary)==expected,"First compiled binary differs")
                files["bin/"+name]=binary.read_bytes()
            policy=json.loads(files["config/worker-isolation-release.v1.json"])
            manifest={"source":native,"packaging_source":head,"target":lock["target"],"programs":list(PLATFORM_PROGRAMS),"wasm_enabled":policy["wasm_enabled"],
                "isolation_report_version":policy["observation_version"],"gpu_supply_from_diagnostic":policy["supply_authorized_by_report"],
                "sha256":{n:hashlib.sha256(v).hexdigest() for n,v in files.items()}}
            files["manifest.json"]=json.dumps(manifest).encode()
            files["SHA256SUMS"]="".join(hashlib.sha256(v).hexdigest()+"  "+n+"\n" for n,v in sorted(files.items())).encode()
            for name,content in files.items():
                out=package/name;out.parent.mkdir(parents=True,exist_ok=True);out.write_bytes(content)
                if name.startswith("bin/"):out.chmod(0o755)
            subprocess.run([sys.executable,"-I",package/"verify.py"],check=True)
            report=smoke(["docker","--context",a.context],package,lock)
            validate_smoke(report)
            print(json.dumps({"native":native,"verifier":head,"single_build":True,"smoke":report},sort_keys=True))
    else:
        dist=ROOT/".local/linux-platform-distributions"
        records=[json.loads(p.read_bytes()) for p in dist.glob("*.json")]
        record,=[r for r in records if r["native_source"]==native and r["packaging_source"]==head]
        archive=dist/record["archive"]
        require(archive.parent==dist and not archive.is_symlink() and digest(archive)==record["sha256"] and archive.stat().st_size==record["bytes"],"Changed package")
        builds=record["builds"]
        require(len(builds)==2 and [v["pass"] for v in builds]==[1,2] and builds[0]["binaries"]==builds[1]["binaries"] and
                all(v==json.loads((base/str(v["pass"])/"build.json").read_bytes()) and v["source"]==native and v["network"]=="none" for v in builds)
                and record["identical_archives"]==2 and record["target"]==lock["target"],"Two independently locked builds required")
        validate_smoke(record["smoke"])
        print(json.dumps({"archive":archive.name,"source":native,"packaging":head,"sha256":record["sha256"],"smoke":record["smoke"]},sort_keys=True))
    print("PASS Linux arm64 device enrollment, confirmed PEM recovery, real gRPC Welcome and withdrawal in the isolated namespace")

if __name__=="__main__":
    main()
