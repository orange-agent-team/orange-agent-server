#!/usr/bin/env python3
"""Execute exact original PRD-001 catalogue, role, and membership acceptance on a disposable PG database."""
import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from owned_postgres import database

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--check', choices=['catalogue','permissions','lifecycle'], default='catalogue')
parser.add_argument('--database-url-file', type=Path, default=root/'.local/database-url')
parser.add_argument('--identity-config', type=Path, default=root/'.local/identity/user-config.json')
args = parser.parse_args()

rustup = shutil.which('rustup') or next((str(p) for p in [Path.home()/'.cargo/bin/rustup',Path('/opt/homebrew/opt/rustup/bin/rustup')] if p.is_file()),None)
if not rustup:
    raise SystemExit('Rustup is required to select the repository toolchain')
# Resolve in the repository so rustup reads its checked-in rust-toolchain.toml.
cargo = Path(subprocess.check_output([rustup,'which','cargo'],cwd=root,text=True).strip())

protoc = os.environ.get('PROTOC') or shutil.which('protoc') or next((str(p) for p in [Path('/opt/homebrew/bin/protoc'),Path('/usr/local/bin/protoc')] if p.is_file()),None)
if not protoc or subprocess.check_output([protoc,'--version'],text=True).strip() != 'libprotoc 36.2':
    raise SystemExit('The pinned Protobuf compiler 36.2 is required for the control-plane integration tests')

def exact(env, suite, case):
    command = [str(cargo),'test','--locked','--offline','-p','orange-control-plane','--features','integration-tests','--test',suite,case,'--','--exact']
    result = subprocess.run(command, cwd=root, env=env, check=True, text=True, stdout=subprocess.PIPE)
    print(result.stdout, end='', flush=True)
    if f'test {case} ... ok' not in result.stdout:
        raise SystemExit('Required PostgreSQL case did not execute: '+case)

with database(args.database_url_file, 'orange_ownership_') as (_, url):
    env = {k:v for k,v in os.environ.items() if not k.startswith('ORANGE_')}
    env.update(PROTOC=protoc,PATH=str(cargo.parent)+os.pathsep+env.get('PATH',''),ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve()))
    if args.check == 'catalogue':
        exact(env,'postgres','resource_catalogue_resolves_each_business_owner_on_the_migrated_database')
    elif args.check == 'permissions':
        exact(env,'teams','five_roles_require_explicit_resource_grants_and_cannot_read_other_tenants')
        operations = json.loads((root/'contracts/openapi/admin.v1.json').read_bytes())['paths']['/admin/v1/actions']['post']['x-action-policies']
        governed = {r['action_type']:r for r in operations}
        required = {'node.quarantine':('nodes.quarantine','single_operator',None),
                    'payment.refund':('payments.refund','maker_checker','payments.refund.approve'),
                    'pricing.publish':('pricing.publish','maker_checker','pricing.publish.approve')}
        for operation,policy in required.items():
            actual=governed[operation]
            if (actual['permission'],actual['approval'],actual['approval_permission']) != policy:
                raise SystemExit('Unexpected governance contract: '+operation)
        print('Verified application resource ACLs and three declared Admin action policies. No real merchant action.',flush=True)
    else:
        exact(env,'teams','rejoining_does_not_restore_content_acl_and_admin_cannot_promote_or_exceed_capacity')
        exact(env,'teams','concurrent_owner_removal_cannot_remove_last_owner_and_replay_checks_current_authority')
        command=[sys.executable,'scripts/check-user-jobs.py']
        required={'job_metadata_lists_only_current_submitter_without_restoring_deleted_content',
                  'membership_changes_and_owner_restriction_preserve_original_job_and_ledger_identity'}
        with subprocess.Popen(command,cwd=root,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True) as process:
            for line in process.stdout:
                print(line,end='',flush=True)
                if line.startswith('test input_cases::job_queries::') and line.rstrip().endswith(' ... ok'):
                    required.discard(line.split('::')[-1].split()[0])
            if process.wait() or required:
                raise SystemExit('Missing verified Job lifecycle cases: '+repr(required))
