#!/usr/bin/env python3
"""Check regional/admin scope and role expiry at the real PostgreSQL audit barrier."""
import argparse
import os
import hashlib
import tempfile
import json
from pathlib import Path
import shutil
import subprocess

from owned_postgres import database
ROOT=Path(__file__).resolve().parents[1]

def main():
    cli=argparse.ArgumentParser();cli.add_argument("--environment",type=Path,default=ROOT/".local")
    private=cli.parse_args().environment.resolve()
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","cargo"],cwd=ROOT,text=True).strip())
    with tempfile.TemporaryDirectory(prefix="orange-issuer-observation-") as temp, database(private/"database-url","orange_issuer_directory_") as (_,url), database(private/"database-url","orange_issuer_upgrade_") as (_,old):
        folder=Path(temp)
        openssl="/opt/homebrew/opt/openssl@3/bin/openssl"
        if not Path(openssl).is_file():openssl=shutil.which("openssl")
        def crypto(*params):return subprocess.check_output([openssl,*params],cwd=folder,stderr=subprocess.PIPE)
        crypto("req","-x509","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-batch","-days","1","-subj","/CN=Private upgrade issuer",
               "-addext","basicConstraints=critical,CA:TRUE","-addext","keyUsage=critical,keyCertSign,cRLSign","-keyout","ca.key","-out","ca.pem")
        ca=folder/"ca.pem";ca.chmod(0o600);digest=hashlib.sha256(crypto("x509","-in","ca.pem","-outform","DER")).hexdigest()
        (folder/"public.pem").write_bytes(crypto("x509","-in","ca.pem","-noout","-pubkey"))
        pubhash=hashlib.sha256(crypto("pkey","-pubin","-in","public.pem","-outform","DER")).hexdigest()
        cfg=folder/"issuer.json";cfg.write_text(json.dumps(dict(version=1,environment="development",region_id="legacy-region",ca_certificate_file=str(ca),
               ca_key_file=str(folder/"ca.key"),ca_certificate_sha256=digest)));cfg.chmod(0o600)
        env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
        env.update(PATH=str(cargo.parent)+os.pathsep+env["PATH"],CARGO_NET_OFFLINE="true",ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_ISSUER_UPGRADE_URL=old,ORANGE_TEST_ISSUER_FILE=str(cfg),
                   ORANGE_TEST_ISSUER_SHA=digest,ORANGE_TEST_ISSUER_SPKI=pubhash,
                   ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"))
        args=[cargo,"test","--locked","--offline","-p","orange-control-plane","--features","issuer-directory-tests","--test","admin_identity","issuer_directory::","--","--test-threads=1"]
        result=subprocess.run(args,cwd=ROOT,env=env,stdout=subprocess.PIPE,text=True)
        print(result.stdout,flush=True);result.check_returncode()
        if "3 passed; 0 failed" not in result.stdout or "historical_issuer_mapping_requires_verified_pinned_ca_after_schema81_upgrade ... ok" not in result.stdout:raise ValueError("Missing scope or legacy upgrade proof")
    print("PASS issuer directory scope isolation, historical unknown keys and expiring audit authority")

if __name__=="__main__":main()
