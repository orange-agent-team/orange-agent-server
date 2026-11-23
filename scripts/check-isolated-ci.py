#!/usr/bin/env python3
"""Run the Rust domain and public protobuf quick checks in the fixed, offline Linux contribution container."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import secrets
import selectors
import shutil
import subprocess
import tarfile
import tempfile
import time
from zipfile import ZipFile

from linux_platform_inputs import local_image_id

ROOT=Path(__file__).resolve().parents[1]
DOCKER=[shutil.which("docker") or "/usr/local/bin/docker","--context","colima-orange-agent"]
TARGETS=("crates/domain/src/lib.rs","contracts/proto/worker.proto","contracts/proto/host.proto")
BOOT=r"""
set -euo pipefail
umask 077
cd /work
mkdir home
tar -xf -
test "$(id -u)" = 65532
test "$(rustc -V | cut -d' ' -f2)" = "$ORANGE_RUST"
test "$(./protoc --version)" = "libprotoc 36.2"
rustc --edition=2024 --test domain.rs -o domain-tests
./domain-tests --test-threads=1 > test-results
cat test-results
grep -q '^test tests::published_billing_examples_and_fee_conservation ... ok$' test-results
grep -q '^test tests::cancel_wins_before_completion_and_old_authorities_fail ... ok$' test-results
./protoc --proto_path=. --proto_path=include --include_imports --descriptor_set_out=result.pb worker.proto host.proto
actual="$(sha256sum result.pb | cut -d' ' -f1)"
if [[ "$actual" != "$ORANGE_WIRE_SHA" ]]; then echo 'REJECT protocol descriptor differs' >&2; exit 19; fi
echo 'PASS domain and wire'
"""


def git(*args):
    return subprocess.check_output(["git",*args],cwd=ROOT)


def run(commit,policy,archive,inject):
    wire=json.loads(git("show",commit+":crates/protocol/generation.lock.json"))
    build=local_image_id(DOCKER,policy["build_image"])
    if not build:
        raise RuntimeError("Pinned local Linux build image required")
    with archive.open("rb") as stream:
        digest=hashlib.file_digest(stream,"sha256").hexdigest()
    if policy["protoc"]["digest"]!="sha256:"+digest or policy["protoc"]["size"]!=archive.stat().st_size:
        raise ValueError("Protobuf input differs from its release")
    with ZipFile(archive) as bundle:
        compiler=bundle.read("bin/protoc")
        well_known={n:bundle.read(n) for n in bundle.namelist() if n.startswith("include/google/protobuf/") and n.endswith(".proto")}
    files={filename:git("show",commit+":"+src) for filename,src in zip(("domain.rs","worker.proto","host.proto"),TARGETS)}
    if inject=="wire":
        files["worker.proto"]+=b"\nmessage CiDriftExample { string version = 1; }\n"
    if inject=="rust":
        files["domain.rs"]+=b'\n#[test] fn ci_expected_negative_fixture(){ assert_eq!(1_u32,2_u32); }\n'
    owner=secrets.token_hex(12)
    with tempfile.TemporaryFile() as payload:
        with tarfile.open(fileobj=payload,mode="w") as tar:
            for name,data in {**files,**well_known,"protoc":compiler}.items():
                info=tarfile.TarInfo(name)
                info.size=len(data)
                info.mode=0o755 if name=="protoc" else 0o644
                tar.addfile(info,io.BytesIO(data))
        payload.seek(0)
        raw=subprocess.check_output([*DOCKER,"create","-i","--pull","never","--label","org.orange-agent.ci="+owner,
                "--network","none","--read-only","--cap-drop","ALL","--security-opt","no-new-privileges",
                "--pids-limit","128","--cpus","1","--memory","768m","--user","65532:65532",
                "--tmpfs","/work:rw,exec,nosuid,nodev,size=256m,mode=1777","--workdir","/work",
                "-e","HOME=/work/home","-e","TMPDIR=/work","-e","ORANGE_RUST="+policy["rust_version"],
                "-e","ORANGE_WIRE_SHA="+wire["descriptor"],"--entrypoint","/bin/bash",build,"-c",BOOT],text=True).strip()
        if not re.fullmatch("[0-9a-f]{64}",raw):
            raise RuntimeError("Missing owned CI container identity")
        try:
            info=json.loads(subprocess.check_output([*DOCKER,"inspect",raw]))[0]
            host=info["HostConfig"]
            if (info["Config"]["Labels"].get("org.orange-agent.ci")!=owner or host["NetworkMode"]!="none" or
                not host["ReadonlyRootfs"] or host.get("DeviceRequests") or host.get("Devices") or host.get("Binds") or
                any(m["Type"]!="tmpfs" for m in info["Mounts"])):
                raise RuntimeError("Unexpected contribution runtime permissions")
            child=subprocess.Popen([*DOCKER,"start","-ai",raw],stdin=payload,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
            try:
                deadline=time.monotonic()+90
                data=bytearray()
                with selectors.DefaultSelector() as selector:
                    selector.register(child.stdout,selectors.EVENT_READ)
                    while timeout:=max(0,deadline-time.monotonic()):
                        if not selector.select(timeout):
                            break
                        block=os.read(child.stdout.fileno(),65536)
                        if not block:
                            break
                        data.extend(block)
                        if len(data)>1048576:
                            raise RuntimeError("CI output limit exceeded")
                if time.monotonic()>=deadline:
                    raise TimeoutError("Contribution job exceeded its deadline")
                result=child.wait(timeout=5)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
                child.stdout.close()
            transcript=data.decode(errors="replace")
            exit_code=json.loads(subprocess.check_output([*DOCKER,"inspect",raw]))[0]["State"]["ExitCode"]
            passed="PASS domain and wire" in transcript.splitlines()
            if inject is None:
                ok=result==exit_code==0 and passed
            elif inject=="wire":
                ok=result==exit_code==19 and not passed and "REJECT protocol descriptor differs" in transcript.splitlines()
            else:
                ok=result==exit_code!=0 and not passed and "ci_expected_negative_fixture ... FAILED" in transcript
            if not ok:
                raise RuntimeError("Unexpected CI verdict: "+transcript[-2000:])
            return {"mutation":inject,"exit_code":exit_code,"quick_gate_passed":passed,
                    "successful_domain_cases":len(re.findall(r"^test \S+ \.\.\. ok$",transcript,re.M))}
        finally:
            subprocess.run([*DOCKER,"rm","-f",raw],check=True,stdout=subprocess.DEVNULL)


def main():
    p=argparse.ArgumentParser()
    p.add_argument("--inputs",type=Path,default=ROOT/".local/linux")
    p.add_argument("--faults",action="store_true")
    args=p.parse_args()
    commit=git("rev-parse","HEAD").decode().strip()
    policy=json.loads(git("show",commit+":config/linux-platform-arm64.lock.json"))
    outcomes=[run(commit,policy,args.inputs/policy["protoc"]["name"],m) for m in ([None,"wire","rust"] if args.faults else [None])]
    print(json.dumps({"source":commit,"target":policy["target"],"build_image":policy["build_image"]["image"],"outcomes":outcomes}))


if __name__=="__main__":
    main()
