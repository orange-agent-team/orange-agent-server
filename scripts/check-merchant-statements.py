#!/usr/bin/env python3
"""Check daily sandbox statements using an isolated PG database and a loopback merchant."""
import argparse
import os
import shutil
import subprocess
from pathlib import Path
from admin_contract_consistency import check_contract
from owned_postgres import database
from acceptance_origins import reserve_origins
import sys
import json
import hashlib

root = Path(__file__).resolve().parents[1]
p = argparse.ArgumentParser()
p.add_argument('--database-url-file',type=Path,default=root/'.local/database-url')
p.add_argument('--identity-dir',type=Path,default=root/'.local/identity')
p.add_argument('--admin-repo',type=Path)
args = p.parse_args()
check_contract(json.loads((root/'contracts/openapi/admin.v1.json').read_bytes()),
               json.loads((root/'contracts/json-schema/admin.schema.json').read_bytes()))
browser={}
if args.admin_repo:
    reserve_origins(43112)
    admin=args.admin_repo.resolve();node=Path.home()/'.nvm/versions/node/v22.23.2/bin/node'
    for spec in json.loads((admin/'docs/contracts/snapshot.json').read_bytes())['files'].values():
        if hashlib.sha256((root/spec['source']).read_bytes()).hexdigest()!=spec['sha256']:raise SystemExit('Mismatched Admin contract snapshot')
    subprocess.run([node,admin/'scripts/check.mjs'],cwd=admin,check=True)
    subprocess.run([sys.executable,admin/'scripts/local-tls.py'],cwd=admin,check=True)
    evidence=root/'.local/statement-evidence';evidence.mkdir(mode=0o700,parents=True,exist_ok=True)
    browser=dict(ORANGE_STATEMENT_BROWSER=str(admin/'apps/admin/scripts/real-merchant-statement.mjs'),ORANGE_STATEMENT_NODE=str(node),ORANGE_STATEMENT_EVIDENCE=str(evidence))

cargo = Path(subprocess.check_output([shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup','which','cargo'],cwd=root,text=True).strip())
with database(args.database_url_file,'orange_statement_') as (_,url):
    env = {k:v for k,v in os.environ.items() if not k.startswith(('STRIPE_','ORANGE_'))}
    env.update(**browser,ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str((args.identity_dir/'user-config.json').resolve()),ORANGE_TEST_OIDC_CONFIG_FILE=str((args.identity_dir/'admin-config.json').resolve()),PATH=str(cargo.parent)+os.pathsep+env['PATH'])
    test='bounded_merchant_scan_survives_lost_claim_partial_failure_and_overlapping_days'
    result=subprocess.run([cargo,'test','--offline','--locked','-p','orange-control-plane','--features','integration-tests','--test','merchant_statement',test,'--','--exact','--nocapture'],cwd=root,env=env,text=True,stdout=subprocess.PIPE)
    print(result.stdout,end='',flush=True)
    result.check_returncode()
    if test not in result.stdout or 'test result: ok. 1 passed; 0 failed' not in result.stdout:raise SystemExit('Required merchant statement case did not execute')
