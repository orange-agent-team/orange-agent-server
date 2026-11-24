#!/usr/bin/env python3
"""Show the environment contract or rehearse a contributor's first build, Job and cleanup."""
import argparse
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
from check_environment_tools import doctor,ToolIssue
from environment_topology import topology

ROOT=Path(__file__).resolve().parents[1]


def mapping():
    values=json.loads((ROOT/"config/environment-names.json").read_bytes())
    regions=json.loads((ROOT/"config/service-regions.json").read_bytes())
    topology(values,regions,"reference")
    return values


def rehearse(tooling,environment):
    if environment!="test":
        raise ValueError("First-run rehearsal requires a fresh test namespace")
    seq=secrets.token_hex(7)
    commit=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    staging=Path(tempfile.mkdtemp(prefix="oa-run-",dir="/private/tmp" if sys.platform=="darwin" else None)).resolve()
    tree=staging/"source"
    tree.mkdir()
    env={k:v for k,v in os.environ.items() if k in {"PATH","HOME","USER","LOGNAME","LANG","LC_ALL","TMPDIR","SDKROOT","DEVELOPER_DIR",
                                                    "ORANGE_POSTGRES_BIN","PROTOC","CARGO_HOME","RUSTUP_HOME"}}
    with socket.socket() as left,socket.socket() as right:
        left.bind(("127.0.0.1",0))
        right.bind(("127.0.0.1",0))
        pgport,idport=str(left.getsockname()[1]),str(right.getsockname()[1])
    env.update(ORANGE_DEVELOPMENT_INSTANCE="first-"+seq,ORANGE_DEVELOPMENT_POSTGRES_PORT=pgport,
               ORANGE_DEVELOPMENT_IDENTITY_PORT=idport,CARGO_NET_OFFLINE="true",CARGO_INCREMENTAL="0",CARGO_PROFILE_DEV_DEBUG="0",
               CARGO_TARGET_DIR=str(staging/"target"),CARGO_BUILD_JOBS="3",RUST_TEST_THREADS="2")
    def call(tool,*args):
        subprocess.run([sys.executable,tree/"scripts"/tool,*args],cwd=tree,env=env,check=True,timeout=1200)
    started=False
    try:
        archive=staging/"source.tar"
        subprocess.run(["git","archive","--format=tar","-o",archive,commit],cwd=ROOT,env=env,check=True)
        with tarfile.open(archive) as tar:
            tar.extractall(tree,filter="data")
        if (tree/".local").exists() or Path(env["CARGO_TARGET_DIR"]).exists():
            raise ValueError("First-run inputs must not contain development state")
        started=True
        call("development-readiness.py","--start","--wait-seconds","15")
        call("check-development-lifecycle.py","--section","worker")
        call("check-development-readiness.py")
    finally:
        try:
            if started:
                call("local-identity.py","remove")
                if (tree/".local/postgres/data/postmaster.pid").exists():
                    call("local-postgres.py","stop")
                verdict=subprocess.run([sys.executable,tree/"scripts/development-readiness.py"],cwd=tree,env=env,capture_output=True)
                if verdict.returncode!=1:
                    raise ValueError("Disposed namespace still available or configuration unexpectedly invalid")
        except BaseException:
            print("Private first-run folder retained for cleanup:",staging,file=sys.stderr)
            raise
        shutil.rmtree(staging)
    return {"source":commit,"environment":"test","fresh_source":True,"fresh_target":True,"actual_local_cluster_and_realm":True,
            "ephemeral_mtls_worker":True,"output_model":"deterministic fixture","isolated_database_removed":True,"cleanup_confirmed":True,
            "tools":tooling}


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("action",choices=["map","topology","doctor","verify"])
    cli.add_argument("--environment",choices=["development","test","staging","production"],default="test")
    cli.add_argument("--run-id",help="Substitute this test run in the read-only topology")
    args=cli.parse_args()
    if args.run_id is not None and args.action!="topology":cli.error("--run-id is for topology only")
    values=mapping()
    if args.action=="topology":
        try:
            report=topology(values,json.loads((ROOT/"config/service-regions.json").read_bytes()),args.run_id if args.run_id is not None else "reference")
        except ValueError as exc:
            cli.error(str(exc))
    elif args.action=="map":
        report=values
    else:
        if args.environment not in ("test","development") or (args.action=="verify" and args.environment!="test"):
            cli.error("This operation only qualifies local development and disposable test resources")
        try:
            tooling=doctor(ROOT)
        except ToolIssue as problem:
            print(json.dumps({"code":"LOCAL_PREREQUISITE","component":problem.tool,"recovery":problem.remedy}),file=sys.stderr)
            sys.exit(2)
        report=tooling if args.action=="doctor" else rehearse(tooling,args.environment)
    print(json.dumps(report,ensure_ascii=False),flush=True)


if __name__=="__main__":
    main()
