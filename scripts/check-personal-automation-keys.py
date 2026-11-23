#!/usr/bin/env python3
"""Verify user-session management and scoped bearer reads on an isolated PostgreSQL database."""
import argparse,json,os,shutil,subprocess,sys
from pathlib import Path
from owned_postgres import database
ROOT=Path(__file__).resolve().parents[1]
cli=argparse.ArgumentParser()
cli.add_argument("--environment",type=Path,default=ROOT/".local")
cli.add_argument("--web-repo",type=Path)
args=cli.parse_args();private=args.environment.resolve()
cargo=Path(subprocess.check_output([shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup",
                                   "which","--toolchain","1.97.1","cargo"],text=True).strip())
env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
env.update(PATH=str(cargo.parent)+os.pathsep+env["PATH"],CARGO_NET_OFFLINE="true",ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"))
schema=json.loads((ROOT/"contracts/json-schema/account.schema.json").read_bytes())["$defs"]
contract=json.loads((ROOT/"contracts/openapi/account.v1.json").read_bytes())
if schema!=json.loads(json.dumps(contract["components"]["schemas"]).replace("#/components/schemas/","#/$defs/")):
    raise ValueError("Account OpenAPI and JSON Schema disagree")
if args.web_repo:
    from acceptance_origins import reserve_origins
    reserve_origins(43122)
    web=args.web_repo.resolve()
    node=Path.home()/".nvm/versions/node/v22.23.2/bin/node"
    subprocess.run([node,web/"scripts/check.mjs"],cwd=web,check=True)
    subprocess.run([sys.executable,web/"scripts/local-tls.py"],cwd=web,check=True)
    env.update(ORANGE_TEST_KEY_BROWSER=str(web/"apps/web/scripts/real-personal-keys.mjs"),ORANGE_TEST_KEY_NODE=str(node))
cases=["a_rotation_queued_on_an_expiring_predecessor_creates_no_new_authority","replacement_retires_once_and_an_uncommitted_successor_cannot_retire_anything","personal_key_lifecycle_binds_owner_scope_and_original_intent","budget_key_never_outlives_expiry_or_withdrawn_account_authority","key_read_rechecks_database_time_after_an_authority_lock_wait","logout_keeps_delegation_and_user_disablement_permanently_withdraws_it"]
if args.web_repo:cases.append("real_personal_key_browser_management")
with database(private/"database-url","orange_personal_keys_") as (_,url):
    env["ORANGE_TEST_DATABASE_URL"]=url
    for case in cases:
        name="personal_key_cases::"+case
        result=subprocess.run([cargo,"test","--locked","--offline","-p","orange-control-plane","--features","personal-key-browser-tests" if case=="real_personal_key_browser_management" else "integration-tests",
                               "--test","user_identity",name,"--","--exact"],cwd=ROOT,env=env,text=True,stdout=subprocess.PIPE)
        print(result.stdout,flush=True);result.check_returncode()
        if name+" ... ok" not in result.stdout or "1 passed; 0 failed" not in result.stdout:raise RuntimeError("Missing named acceptance "+name)
print("PASS personal key ownership, idempotency, scope, expiry and revocation on real PostgreSQL")
