#!/usr/bin/env python3
"""Actual PG/HTTP personal account boundaries. Explicit session fixtures, not OIDC certification."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
env=os.environ.copy();env['PATH']=str(cargo.parent)+os.pathsep+env.get('PATH','')
if not subprocess.check_output([cargo.parent/'rustc','--version'],env=env,text=True).startswith('rustc 1.97.1 '):raise SystemExit('Pinned server Rust1.97.1 required')
if not env.get('ORANGE_TEST_DATABASE_URL'):env['ORANGE_TEST_DATABASE_URL']=(ROOT/'.local/database-url').read_text().strip()
env.setdefault('ORANGE_TEST_USER_CONFIG_FILE',str(ROOT/'.local/identity/user-config.json'))
schema=json.loads((ROOT/'contracts/json-schema/account.schema.json').read_text())
api=json.loads((ROOT/'contracts/openapi/account.v1.json').read_text())
if schema['$defs']!=json.loads(json.dumps(api['components']['schemas']).replace('#/components/schemas/','#/$defs/')):raise SystemExit('Account OpenAPI/schema mismatch')
command=[str(cargo),'test','-p','orange-control-plane','--features','integration-tests','--test','user_identity','account_cases','--locked','--','--test-threads=1']
passed=False
with subprocess.Popen(command,cwd=ROOT,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True) as child:
    for line in child.stdout:
        print(line,end='',flush=True)
        if re.search(r'test result: ok\. [1-9][0-9]* passed; 0 failed;',line):passed=True
    code=child.wait()
if code:raise SystemExit(code)
if not passed:raise SystemExit('No account tests ran; empty filter is not success')
print('PASS actual personal-budget/zero-wallet HTTP transactions, exact amounts, current-session and expiry rollback; explicit session fixtures, no balance minting or payment certification')
