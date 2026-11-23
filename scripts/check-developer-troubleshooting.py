#!/usr/bin/env python3
"""Prove the documented failure signals on a disposable DB, then restore and rotate only fixture credentials."""
import argparse
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
from urllib.parse import urlsplit
from owned_postgres import database
from platform_package_smoke import listening
from importlib.util import module_from_spec,spec_from_file_location

ROOT=Path(__file__).resolve().parents[1]


def require(value,reason):
    if not value:
        raise ValueError(reason)


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--environment",type=Path,default=ROOT/".local")
    args=cli.parse_args()
    private=args.environment.resolve()
    salt="private-"+secrets.token_hex(16)
    env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_") and k!="DATABASE_URL"}
    result=subprocess.run([sys.executable,ROOT/"scripts/developer-first-run.py","doctor"],
                          env=dict(env,PROTOC="/nonexistent/"+salt),capture_output=True,text=True,timeout=20)
    require(result.returncode==2 and json.loads(result.stderr)["component"]=="protoc" and salt not in result.stderr,"Tool failure isn't actionable or redacted")
    for stage in ("staging","production"):
        result=subprocess.run([sys.executable,ROOT/"scripts/developer-first-run.py","verify","--environment",stage],
                              env=env,capture_output=True,text=True)
        require(result.returncode==2,"External rehearsal scope allowed")
    subprocess.run([sys.executable,ROOT/"scripts/development-readiness.py","--environment",private],check=True)
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
    env.update(PATH=str(cargo.parent)+os.pathsep+env["PATH"],CARGO_NET_OFFLINE="true")
    subprocess.run([cargo,"build","--locked","--offline","-p","orange-control-plane","--bin","orange-api"],cwd=ROOT,env=env,check=True)
    out=json.loads(subprocess.check_output([cargo,"metadata","--no-deps","--format-version","1","--locked","--offline"],cwd=ROOT,env=env))["target_directory"]
    binary=Path(out)/"debug/orange-api"
    spec=spec_from_file_location("owned_lifecycle",ROOT/"scripts/check-development-lifecycle.py")
    lifecycle=module_from_spec(spec)
    spec.loader.exec_module(lifecycle)
    with database(private/"database-url","orange_handbook_") as (_,url), tempfile.TemporaryDirectory() as folder:
        directory=Path(folder)
        identity=directory/"identities.json"
        def rotate(secret):
            pending=directory/"identities.tmp"
            pending.write_text(json.dumps([{"owner_id":"handbook-owner","token":secret}]))
            pending.chmod(0o600)
            pending.replace(identity)
        rotate(salt)
        app=dict(env,ORANGE_ENV="development",ORANGE_DEV_IDENTITIES_FILE=str(identity),DATABASE_URL=url,ORANGE_BIND="127.0.0.1:0")
        def rejected(env,category):
            child=subprocess.run([binary],env=env,capture_output=True,text=True,timeout=20)
            require(child.returncode!=0 and salt not in child.stderr and url not in child.stderr and "listening on" not in child.stderr,"Unsafe startup refusal")
            failure=json.loads(child.stderr)
            require(failure["class"]==category and not failure["retryable"],"Wrong failure class: "+str(failure))
            return failure
        with socket.socket() as reserved:
            reserved.bind(("127.0.0.1",0))
            reserved.listen()
            rejected(dict(app,ORANGE_BIND="127.0.0.1:"+str(reserved.getsockname()[1])),"conflict")
        sql=lambda query:lifecycle.query(url,query)
        original=sql("SELECT encode(checksum,'hex') FROM _sqlx_migrations ORDER BY version LIMIT 1")
        require(len(original)==96 and all(c in "0123456789abcdef" for c in original),"Expected applied SQLx SHA-384")
        sql("CREATE TABLE handbook_marker(value integer); INSERT INTO handbook_marker VALUES(37)")
        sql("UPDATE _sqlx_migrations SET checksum=decode(repeat('ff',48),'hex') WHERE version=(SELECT min(version) FROM _sqlx_migrations)")
        failure=rejected(app,"migration")
        require(failure["code"]=="STARTUP_MIGRATION_FAILED" and failure["recovery"]=="inspect_migration_history","Missing migration recovery signal")
        sql("UPDATE _sqlx_migrations SET checksum=decode('"+original+"','hex') WHERE version=(SELECT min(version) FROM _sqlx_migrations)")
        def get(origin,token):
            port=urlsplit(origin).port
            conn=http.client.HTTPConnection("127.0.0.1",port,timeout=4)
            try:
                conn.request("GET","/api/v1/budgets/nonexistent",headers={"Authorization":"Bearer "+token})
                resp=conn.getresponse()
                code=json.loads(resp.read())["code"]
                return resp.status,code
            finally:
                conn.close()
        with listening(binary,app,"development API") as origin:
            require(get(origin,salt)==(404,"NOT_FOUND"),"Restored deployment did not authenticate the first principal")
        replacement=secrets.token_hex(32)
        rotate(replacement)
        with listening(binary,app,"development API") as origin:
            require(get(origin,salt)[0]==401 and get(origin,replacement)==(404,"NOT_FOUND"),"Rotation failed or expanded access")
        require(sql("SELECT value FROM handbook_marker")=="37","Repair damaged unrelated data")
    print(json.dumps({"tool_error_redacted":True,"external_profiles_denied":True,"port_conflict":True,"corrupt_migration_denied":True,
                      "restored_release_started":True,"old_credential_rejected":True,"replacement_authenticated":True,"unrelated_data_retained":True}))


if __name__=="__main__":
    main()
