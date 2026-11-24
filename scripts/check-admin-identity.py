#!/usr/bin/env python3
"""Run the Admin OIDC, TOTP, grant-revocation and approval suite on an owned PostgreSQL database."""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit

from acceptance_origins import reserve_origins
from admin_fixture_roles import provision
from identity_schema_version import schema_version
from owned_postgres import database

ROOT = Path(__file__).resolve().parents[1]


def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("--admin-repo", required=True, type=Path)
    cli.add_argument("--node", default=shutil.which("node"))
    cli.add_argument("--prepared-environment", type=Path)
    cli.add_argument("--server-repo", type=Path, default=ROOT, help="Service code to build and test")
    cli.add_argument("--admin-driver", type=Path, help="Separate browser fixture for an older Admin checkout with the same identity contract")
    args = cli.parse_args()
    server = args.server_repo.resolve(strict=True)
    admin = args.admin_repo.resolve(strict=True)
    driver = args.admin_driver.resolve(strict=True) if args.admin_driver else admin
    for name in ("docs/contracts/admin.schema.json", "apps/admin/src/api/contracts.ts"):
        if (admin/name).read_bytes() != (driver/name).read_bytes():
            raise SystemExit("Browser fixture and Admin identity contract differ")
    if not args.node or subprocess.check_output([args.node,"--version"], text=True).strip() != "v22.23.2":
        raise SystemExit("Node 22.23.2 required")
    private = (args.prepared_environment or server/".local").resolve()
    rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo = Path(subprocess.check_output([rustup,"which","cargo"],cwd=server,text=True).strip())
    env = dict(os.environ, PATH=os.pathsep.join([str(cargo.parent), str(Path(args.node).parent), os.environ["PATH"]]))
    subprocess.run([cargo,"build","--bin","orange-admin-api","--locked"],cwd=server,env=env,check=True)
    meta = json.loads(subprocess.check_output([cargo,"metadata","--no-deps","--locked","--format-version","1"],cwd=server,env=env))
    binary = Path(meta["target_directory"])/"debug/orange-admin-api"
    reserve_origins(43112)
    for port in (43102,43112):
        with socket.socket() as probe:
            if probe.connect_ex(("127.0.0.1",port)) == 0:
                raise SystemExit(f"Configured identity port {port} is occupied")
    if args.prepared_environment is None:
        subprocess.run([sys.executable,server/"scripts/local-postgres.py","start"],cwd=server,env=env,check=True)
        subprocess.run([sys.executable,server/"scripts/local-identity.py","start"],cwd=server,env=env,check=True)
    db_file = private/"database-url"
    admin_cfg = private/"identity/admin-config.json"
    fixture = private/"identity/fixture.json"
    maintenance = db_file.read_text().strip()
    db = urlsplit(maintenance)
    identity = json.loads(admin_cfg.read_bytes())
    if (db.scheme not in ("postgres","postgresql") or db.hostname!="127.0.0.1" or db.port!=55432 or db.path!="/orange_dev" or db.query or db.fragment
        or identity.get("environment")!="dev" or identity.get("admin_origin")!="https://127.0.0.1:43112"
        or urlsplit(identity["issuer"]).hostname not in ("localhost","127.0.0.1","::1")):
        raise SystemExit("Dedicated local development database and Admin identity required")
    before = schema_version(maintenance,env)
    subprocess.run([sys.executable,admin/"scripts/local-tls.py"],cwd=admin,env=env,check=True)
    with database(db_file,"orange_admin_oidc_") as (name,url), tempfile.TemporaryDirectory(prefix="orange-admin-oidc-") as tmp:
        owned_file = Path(tmp)/"database-url"
        with open(owned_file,"x",opener=lambda p,flags:os.open(p,flags,0o600)) as out:
            out.write(url+"\n")
        env.update(ORANGE_ADMIN_TLS_DIR=str(admin/".local/tls"),ORANGE_TEST_DATABASE_URL=url,
                   ORANGE_TEST_OIDC_CONFIG_FILE=str(admin_cfg),ORANGE_IDENTITY_FIXTURE=str(fixture),
                   ORANGE_TEST_DATABASE_URL_FILE=str(owned_file),ORANGE_TEST_ADMIN_OIDC_DATABASE=name)
        subprocess.run([cargo,"test","-p","orange-control-plane","--features","integration-tests","--test","admin_identity","--locked"],cwd=server,env=env,check=True)
        provision(urlsplit(url),fixture)
        verify_browser(server,admin,driver,args.node,binary,env,url,admin_cfg)
        level = schema_version(url,env)
        if level!=len(list((server/"services/control-plane/migrations").glob("*.sql"))):
            raise ValueError("Owned schema differs from tested service version")
    print(json.dumps({"server":revision(server),"admin":revision(admin),"owned_schema":level,"owned_database_removed":True,
                      "maintenance_schema_before":before,"maintenance_schema_after":schema_version(maintenance,env)}))
    print("PASS: actual Keycloak password/TOTP + Rust OIDC + PostgreSQL + HTTPS/Chrome; development only")


def revision(repo):
    return subprocess.check_output(["git","rev-parse","HEAD"],cwd=repo,text=True).strip()


def verify_browser(server,admin,driver,node,binary,env,url,admin_cfg):
    processes,logs=[],[]
    logdir=ROOT/".local/admin-e2e"
    logdir.mkdir(parents=True,exist_ok=True,mode=0o700)
    def launch(command,cwd,name,child_env):
        log=open(logdir/name,"w",opener=lambda p,flags:os.open(p,flags,0o600))
        logs.append(log)
        processes.append(subprocess.Popen(command,cwd=cwd,env=child_env,stdout=log,stderr=log))
    def ready(url,expected):
        context=ssl._create_unverified_context()  # The local test origin uses its own certificate.
        for _ in range(100):
            if any(child.poll() is not None for child in processes):
                raise RuntimeError("Owned Admin test process exited; inspect the private logs")
            try:
                with urllib.request.urlopen(url,timeout=1,context=context) as response:status=response.status
            except urllib.error.HTTPError as error:status=error.code
            except OSError:
                time.sleep(.2);continue
            if status==expected:return
            time.sleep(.2)
        raise RuntimeError("Local Admin readiness failed")
    try:
        clean={k:v for k,v in env.items() if not k.startswith(("ORANGE_","PG","STRIPE_")) and k!="DATABASE_URL"}
        launch([binary],server,"api.log",dict(clean,ORANGE_ENV="development",DATABASE_URL=url,
            ORANGE_ADMIN_CONFIG_FILE=str(admin_cfg),ORANGE_ADMIN_BIND="127.0.0.1:43102"))
        ready("http://127.0.0.1:43102/admin/v1/session",401)
        launch([node,admin/"apps/admin/node_modules/vite/bin/vite.js"],admin/"apps/admin","vite.log",dict(clean,ORANGE_ADMIN_TLS_DIR=env["ORANGE_ADMIN_TLS_DIR"]))
        ready("https://127.0.0.1:43112/",200)
        subprocess.run([node,"node_modules/@playwright/test/cli.js","test","--config","playwright.oidc.config.ts"],
                       cwd=driver/"apps/admin",env=env,check=True)
    finally:
        for child in reversed(processes):
            if child.poll() is None:
                child.send_signal(signal.SIGINT)
                try:child.wait(timeout=10)
                except subprocess.TimeoutExpired:child.kill();child.wait()
        for log in logs:log.close()


if __name__=="__main__":main()
