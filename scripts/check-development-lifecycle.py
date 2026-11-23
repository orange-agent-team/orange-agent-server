#!/usr/bin/env python3
"""Exercise real local dependencies, an inference protocol fixture, and disposable namespace recovery."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from urllib.parse import urlsplit,unquote

from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]


def query(url,sql):
    uri=urlsplit(url)
    if uri.hostname!="127.0.0.1" or uri.username!="orange_local":
        raise ValueError("Expected the managed local database")
    env={k:v for k,v in os.environ.items() if not k.startswith("PG")}
    env.update(PGHOST=uri.hostname,PGPORT=str(uri.port),PGDATABASE=uri.path[1:],PGUSER=uri.username,
               PGPASSWORD=unquote(uri.password),PGCONNECT_TIMEOUT="3")
    tool=shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
    return subprocess.check_output([tool,"-X","-w","-At","-v","ON_ERROR_STOP=1","-c",sql],env=env,text=True).strip()


def run(script,*args,env=None,need=()):
    cmd=[sys.executable,ROOT/"scripts"/script,*args]
    pending=set(need)
    with subprocess.Popen(cmd,env=env,cwd=ROOT,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True) as child:
        for line in child.stdout:
            print(line,end="",flush=True)
            for marker in tuple(pending):
                if marker in line:
                    pending.remove(marker)
        if child.wait() or pending:
            raise RuntimeError("Incomplete development validation in "+script+"; missing "+str(sorted(pending)))


def recovery(private):
    conn=private/"database-url"
    base=conn.read_text().strip()
    with database(conn,"orange_sibling_") as (_,sibling):
        query(sibling,"CREATE TABLE retained(id integer); INSERT INTO retained VALUES (37)")
        with database(conn,"orange_restart_") as (old,url):
            query(url,"CREATE TABLE isolated_seed(id integer); INSERT INTO isolated_seed VALUES (41)")
            if query(url,"SELECT id FROM isolated_seed")!="41":
                raise ValueError("Missing seed")
        if query(base,"SELECT count(*) FROM pg_database WHERE datname='"+old+"'")!="0":
            raise ValueError("Old namespace remains live")
        with database(conn,"orange_restart_") as (new,url):
            if new==old or query(url,"SELECT to_regclass('public.isolated_seed') IS NULL")!="t" or query(sibling,"SELECT id FROM retained")!="37":
                raise ValueError("Replacement namespace or sibling isolation failed")
    run("check-development-cache.py")
    print(json.dumps({"database_disposal_confirmed":True,"fresh_namespace":True,"sibling_data_retained":True,"cache_recreated":True}))


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--section",choices=["services","worker","restart"],required=True)
    cli.add_argument("--environment",type=Path,default=ROOT/".local")
    args=cli.parse_args()
    private=args.environment.resolve()
    run("development-readiness.py","--environment",private,"--wait-seconds","5")
    if args.section=="services":
        run("check-development-cache.py")
        run("check-s3-source.py","--database-url-file",private/"database-url","--identity-config",private/"identity/user-config.json",
            "--weed-binary",private/"seaweedfs/weed")
    elif args.section=="worker":
        with database(private/"database-url","orange_dev_worker_") as (_,url):
            env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
            env.update(ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"),
                       ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"),ORANGE_WORKER_OUTPUT_TESTS="1")
            run("check-worker-session.py",env=env,need=["exact_output_bytes_commit_once_are_user_readable_and_never_settle_money ... ok"])
    else:
        recovery(private)
    print("PASS managed development",args.section)


if __name__=="__main__":
    main()
