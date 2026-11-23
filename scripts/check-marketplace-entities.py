#!/usr/bin/env python3
"""Check declared marketplace ownership against actual migrations, ledger and session lifecycle."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]
cli=argparse.ArgumentParser()
cli.add_argument("--section",required=True,choices=["catalogue","types","lifecycle","examples"])
cli.add_argument("--environment",type=Path,default=ROOT/".local")
args=cli.parse_args()
private=args.environment.resolve()
rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
env=dict(os.environ,CARGO_NET_OFFLINE="true",PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"],
         ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"),ORANGE_TEST_OIDC_CONFIG_FILE=str(private/"identity/admin-config.json"))
def rust(args,expected):
    result=subprocess.run([cargo,"test","--locked","--offline",*args],cwd=ROOT,env=env,text=True,stdout=subprocess.PIPE)
    print(result.stdout,flush=True)
    result.check_returncode()
    if any(marker not in result.stdout for marker in expected):
        raise ValueError("Missing required compiled case: "+str(expected))

if args.section=="examples":
    rust(["-p","orange-protocol","--test","marketplace_examples"],["2 passed; 0 failed","four_typed_modalities_and_organized_retry_reconcile ... ok",
        "reject_crossed_relationships_and_invalid_amount_or_duration ... ok"])
    print("PASS marketplace examples")
    sys.exit(0)
if args.section=="types":
    rust(["-p","orange-domain"],["2 passed; 0 failed","compile fail ... ok","identities_preserve_valid_legacy_spelling_and_reject_bad_boundaries ... ok"])
    rust(["-p","orange-protocol","--features","grpc","--lib"],["test execution::tests::","test transcript::","0 failed"])
cases={"types":[("postgres","missing_owner_and_invalid_wire_amounts_create_no_budget_or_postings")],"catalogue":[("postgres","domain_entity_references_match_owned_primary_keys_and_validated_foreign_keys"),
                    ("teams","five_roles_require_explicit_resource_grants_and_cannot_read_other_tenants")],
       "lifecycle":[("user_identity","revoked_session_cannot_authenticate_and_owner_ledger_remains_balanced_and_immutable"),
                    ("postgres","database_enforces_immutable_balanced_ledger")]}
with database(private/"database-url","orange_entity_") as (_,url):
    env["ORANGE_TEST_DATABASE_URL"]=url
    for suite,case in cases[args.section]:
        child=subprocess.run([cargo,"test","--locked","--offline","-p","orange-control-plane","--features","integration-tests",
                              "--test",suite,case,"--","--exact"],cwd=ROOT,env=env,text=True,stdout=subprocess.PIPE)
        print(child.stdout,flush=True)
        child.check_returncode()
        if "test "+case+" ... ok" not in child.stdout or "1 passed; 0 failed" not in child.stdout:
            raise ValueError("Missing exact test "+case)
print("PASS marketplace",args.section,flush=True)
