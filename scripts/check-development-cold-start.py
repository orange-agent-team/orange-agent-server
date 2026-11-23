#!/usr/bin/env python3
"""Provision a fresh private development cluster and realm on isolated loopback ports, then restart both."""
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
from urllib.parse import unquote,urlsplit

ROOT=Path(__file__).resolve().parents[1]


def ports():
    with socket.socket() as a,socket.socket() as b:
        a.bind(("127.0.0.1",0))
        b.bind(("127.0.0.1",0))
        return [str(x.getsockname()[1]) for x in (a,b)]


def main():
    folder=Path(tempfile.mkdtemp(prefix="oa-boot-",dir="/private/tmp" if sys.platform=="darwin" else None)).resolve()
    tree=folder/"source"
    tree.mkdir()
    pgport,idpport=ports()
    profile="cold-"+secrets.token_hex(8)
    env=dict(os.environ,ORANGE_DEVELOPMENT_INSTANCE=profile,ORANGE_DEVELOPMENT_POSTGRES_PORT=pgport,ORANGE_DEVELOPMENT_IDENTITY_PORT=idpport)
    commit=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    archive=folder/"source.tar"
    subprocess.run(["git","archive","-o",archive,commit,"scripts"],cwd=ROOT,check=True)
    with tarfile.open(archive) as tar:
        tar.extractall(tree,filter="data")
    def run(name,*args):
        return subprocess.check_output([sys.executable,tree/"scripts"/name,*args],env=env,cwd=tree,text=True,timeout=300)
    def readiness(*args):
        report=json.loads(run("development-readiness.py",*args))
        if not report["ready"] or report["postgres_port"]!=int(pgport) or report["oidc_port"]!=int(idpport):
            raise ValueError("Unexpected readiness identity")
        return report
    try:
        readiness("--start","--wait-seconds","10")
        private=tree/".local"
        cfg=[private/"database-url",private/"identity/user-config.json",private/"identity/admin-config.json"]
        original=[p.read_bytes() for p in cfg]
        uri=urlsplit(original[0].decode().strip())
        penv={k:v for k,v in env.items() if not k.startswith("PG")}
        penv.update(PGHOST="127.0.0.1",PGPORT=pgport,PGDATABASE="orange_dev",PGUSER="orange_local",PGPASSWORD=unquote(uri.password))
        psql=shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
        def query(sql):
            return subprocess.check_output([psql,"-X","-At","-w","-v","ON_ERROR_STOP=1","-c",sql],env=penv,text=True).strip()
        query("CREATE TABLE boot_probe(id integer); INSERT INTO boot_probe VALUES(37)")
        readiness("--start")
        run("local-identity.py","stop")
        run("local-postgres.py","stop")
        denied=subprocess.run([sys.executable,tree/"scripts/development-readiness.py"],cwd=tree,env=env,capture_output=True)
        if denied.returncode!=1:
            raise ValueError("Stopped dependencies reported ready")
        readiness("--start","--wait-seconds","10")
        if query("SELECT id FROM boot_probe")!="37" or [p.read_bytes() for p in cfg]!=original:
            raise ValueError("Restart changed private configuration or lost persisted fixture data")
        print(json.dumps({"source":commit,"clean_boot":True,"repeat_start_same_identity":True,"stopped_dependencies_denied":True,
                          "actual_cluster_and_realm_restart":True,"credentials_preserved":True,"postgres_seed_preserved":True}))
    finally:
        # Never discard generated cluster state if its owned services could not be stopped.
        run("local-identity.py","remove")
        if (tree/".local/postgres/data/postmaster.pid").exists():
            run("local-postgres.py","stop")
        shutil.rmtree(folder)


if __name__=="__main__":
    main()
