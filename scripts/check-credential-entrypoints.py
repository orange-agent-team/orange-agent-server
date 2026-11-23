#!/usr/bin/env python3
"""Check credential separation at the real local TLS, HTTP and payment verification entry points."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]


def run(command,env,marker):
    result=subprocess.run(command,cwd=ROOT,env=env,text=True,stdout=subprocess.PIPE)
    print(result.stdout,end="",flush=True)
    result.check_returncode()
    if marker+" ... ok" not in result.stdout:raise ValueError("Missing required case: "+marker)


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--environment",type=Path,default=ROOT/".local")
    private=cli.parse_args().environment.resolve()
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","cargo"],cwd=ROOT,text=True).strip())
    env={k:v for k,v in os.environ.items() if not k.startswith(("ORANGE_","STRIPE_"))}
    env.update(PATH=str(cargo.parent)+os.pathsep+env["PATH"],CARGO_NET_OFFLINE="true",
               ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"),ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"))
    base=[cargo,"test","--locked","--offline","-p","orange-control-plane"]
    webhook="payments::webhook::tests::exact_signed_octets_timestamp_and_rotation"
    run([*base,"--lib",webhook,"--","--exact"],env,webhook)
    with database(private/"database-url","orange_credential_roles_") as (_,url):
        env["ORANGE_TEST_DATABASE_URL"]=url
        scope="user_and_admin_session_values_are_refused_by_each_other_after_cookie_rename"
        run([*base,"--features","integration-tests","--test","user_identity",scope,"--","--exact"],env,scope)
        run([sys.executable,"scripts/check-worker-session.py"],dict(env,ORANGE_PEER_PURPOSE_TESTS="1"),
            "absent_foreign_and_unregistered_certificates_never_claim_session")
    print("PASS two distinct browser sessions, real mTLS certificate purpose and exact payment callback signature")


if __name__=="__main__":
    main()
