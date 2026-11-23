#!/usr/bin/env python3
"""Verify locked Rust/protoc generation twice from tracked sources and exercise drift rejection."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile

ROOT=Path(__file__).resolve().parents[1]


def run(args, root, env, expected_error=None):
    result=subprocess.run(args,cwd=root,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
    if expected_error is None:
        if result.returncode:
            raise RuntimeError(result.stdout[-5000:])
    elif not result.returncode or expected_error not in result.stdout:
        raise RuntimeError("Expected drift rejection: "+str(expected_error)+"\n"+result.stdout[-3000:])


def main():
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
    command=[cargo,"check","--locked","--offline","-p","orange-protocol","--features","grpc"]
    commit=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    with tempfile.TemporaryDirectory(prefix="orange-wire-gen-") as work:
        folder=Path(work)
        archive=folder/"input.tar"
        subprocess.run(["git","archive","--format=tar","-o",archive,commit],cwd=ROOT,check=True)
        samples=[]
        for index in (1,2):
            checkout=folder/str(index)
            checkout.mkdir()
            with tarfile.open(archive) as tar:
                tar.extractall(checkout,filter="data")
            env=dict(os.environ,CARGO_NET_OFFLINE="true",PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"],
                     CARGO_TARGET_DIR=str(checkout/"target"),CARGO_INCREMENTAL="0",CARGO_PROFILE_DEV_DEBUG="0")
            run(command,checkout,env)
            compiler=[sys.executable,"scripts/check-proto.py"]
            run(compiler,checkout,env)
            output=checkout/".local/contracts/orange.pb"
            target=checkout/"target/debug/build"
            generated=list(target.glob("orange-protocol-*/out/orange.*"))
            files={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in [*generated,output]}
            if set(files)!={"orange.pb","orange.descriptor.pb","orange.host.v1.rs","orange.worker.v1.rs"}:
                raise ValueError("Incomplete generated artifact set")
            samples.append(files)
        if samples[0]!=samples[1]:
            raise ValueError("Independent build mismatch")
        lock=checkout/"crates/protocol/generation.lock.json"
        original=lock.read_bytes()
        modified=json.loads(original)
        modified["bindings"]["orange.host.v1.rs"]="0"*64
        lock.write_text(json.dumps(modified))
        run(command,checkout,env,"Generated orange.host.v1.rs differs")
        lock.write_bytes(original)
        with (checkout/"contracts/proto/worker.proto").open("a") as f:
            f.write("\nmessage GenerationReviewFixture { string label = 1; }\n")
        run(command,checkout,env,"Generated orange.descriptor.pb differs")
        run(compiler,checkout,env,"Public descriptor differs")
        if hashlib.sha256(output.read_bytes()).hexdigest()!=samples[0]["orange.pb"]:
            raise ValueError("Rejected schema replaced the published descriptor")
        print(json.dumps({"source":commit,"independent_offline_builds":2,"artifacts":samples[0],
                          "binding_drift_rejected":True,"schema_drift_rejected":True,"previous_descriptor_preserved":True}))


if __name__=="__main__":
    main()
