#!/usr/bin/env python3
"""Check immutable quote terms, expiry during admission, and replay of a previously committed reservation."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
from owned_postgres import database

root=Path(__file__).resolve().parents[1]
cli=argparse.ArgumentParser()
cli.add_argument("--environment",type=Path,default=root/".local")
args=cli.parse_args()
rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"],CARGO_NET_OFFLINE="true")
url_file=args.environment.resolve()/"database-url"
for target,case in [
    ("quote_admission","issued_terms_stay_immutable_and_only_new_effects_require_an_unexpired_quote"),
    ("postgres","failed_freeze_and_bad_quotes_leave_no_side_effect"),
    ("postgres","idempotency_checks_content_and_current_authority")
]:
    with database(url_file,"orange_quote_contract_") as (_,url):
        scoped=dict(env,ORANGE_TEST_QUOTE_DATABASE_URL=url,ORANGE_TEST_DATABASE_URL=url)
        result=subprocess.run([cargo,"test","--locked","--offline","-p","orange-control-plane","--features","integration-tests",
                               "--test",target,case,"--","--exact"],cwd=root,env=scoped,text=True,stdout=subprocess.PIPE)
        print(result.stdout,flush=True)
        result.check_returncode()
        if "test "+case+" ... ok" not in result.stdout.splitlines():raise ValueError("Missing test "+case)
print("PASS immutable quote terms, expiry at admission and authorized idempotent replay")
