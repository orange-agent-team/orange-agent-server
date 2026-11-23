#!/usr/bin/env python3
"""Exercise ID type separation in Rust and access control on the authenticated Job endpoint."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument("--environment",type=Path,default=ROOT/".local")
args=parser.parse_args()
private=args.environment.resolve()
rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
env=dict(os.environ,CARGO_NET_OFFLINE="true",PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
result=subprocess.run([cargo,"test","--locked","--offline","-p","orange-domain"],cwd=ROOT,env=env,text=True,stdout=subprocess.PIPE)
print(result.stdout,flush=True)
result.check_returncode()
if "compile fail ... ok" not in result.stdout or "identities_preserve_valid_legacy_spelling_and_reject_bad_boundaries ... ok" not in result.stdout:
    raise ValueError("Missing typed boundary or compile-fail case")
with database(private/"database-url","orange_typed_job_") as (_,url):
    env.update(ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"),
               ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"))
    result=subprocess.run([sys.executable,ROOT/"scripts/check-user-jobs.py"],cwd=ROOT,env=env,text=True,
                          stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
    print(result.stdout,flush=True)
    result.check_returncode()
    if not all(case+" ... ok" in result.stdout for case in (
        "typed_job_status_requires_the_original_owner_job_and_authenticated_submitter",
        "job_metadata_lists_only_current_submitter_without_restoring_deleted_content",
        "membership_changes_and_owner_restriction_preserve_original_job_and_ledger_identity")):
        raise ValueError("Missing real Job query authorization test")
print("PASS typed owner, submitting user and Job; legacy identifiers, invalid path, authentication and foreign identity refusal")
