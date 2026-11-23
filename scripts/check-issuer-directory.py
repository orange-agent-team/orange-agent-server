#!/usr/bin/env python3
"""Check regional/admin scope and role expiry at the real PostgreSQL audit barrier."""
import argparse
import os
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
    with database(private/"database-url","orange_issuer_directory_") as (_,url):
        env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
        env.update(PATH=str(cargo.parent)+os.pathsep+env["PATH"],CARGO_NET_OFFLINE="true",ORANGE_TEST_DATABASE_URL=url,
                   ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"))
        args=[cargo,"test","--locked","--offline","-p","orange-control-plane","--features","issuer-directory-tests","--test","admin_identity","issuer_directory::","--","--test-threads=1"]
        result=subprocess.run(args,cwd=ROOT,env=env,stdout=subprocess.PIPE,text=True)
        print(result.stdout,flush=True);result.check_returncode()
        if "2 passed; 0 failed" not in result.stdout:raise ValueError("Missing both scoped issuer directory tests")
    print("PASS issuer directory scope isolation, historical unknown keys and expiring audit authority")

if __name__=="__main__":main()
