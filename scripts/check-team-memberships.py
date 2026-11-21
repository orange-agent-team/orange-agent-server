#!/usr/bin/env python3
"""Run the actual user-session API and PostgreSQL team membership/invitation cases."""
import argparse
import os
import shutil
import subprocess
from pathlib import Path
from owned_postgres import database
from acceptance_origins import reserve_origins
import sys
root=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('--database-url-file',type=Path,default=root/'.local/database-url')
parser.add_argument('--identity-config',type=Path,default=root/'.local/identity/user-config.json')
parser.add_argument('--web-repo',type=Path)
args=parser.parse_args()
browser={}
if args.web_repo:
    reserve_origins(43122);web=args.web_repo.resolve();node=Path.home()/'.nvm/versions/node/v22.23.2/bin/node'
    subprocess.run([node,web/'scripts/check.mjs'],cwd=web,check=True)
    subprocess.run([sys.executable,web/'scripts/local-tls.py'],cwd=web,check=True)
    evidence=root/'.local/team-acceptance';evidence.mkdir(parents=True,exist_ok=True,mode=0o700)
    browser=dict(ORANGE_TEAM_NODE=str(node),ORANGE_TEAM_DRIVER=str(web/'apps/web/scripts/real-team-governance.mjs'),ORANGE_TEAM_SCREENSHOT=str(evidence/'member-budget.png'))

rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','cargo'],cwd=root,text=True).strip())
with database(args.database_url_file,'orange_team_inv_') as (_,url), database(args.database_url_file,'orange_team_old_') as (_,old):
    env={k:v for k,v in os.environ.items() if not k.startswith('ORANGE_')}
    env.update(**browser,ORANGE_TEAM_OLD_DATABASE=old,ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve()),PATH=str(cargo.parent)+os.pathsep+env['PATH'],PROTOC=env.get('PROTOC') or shutil.which('protoc') or '/opt/homebrew/bin/protoc')
    result=subprocess.run([cargo,'test','--offline','--locked','-p','orange-control-plane','--features','integration-tests','--test','user_identity','team_operations::','--','--test-threads=1'],cwd=root,env=env,text=True,stdout=subprocess.PIPE)
    print(result.stdout,end='',flush=True);result.check_returncode()
    for case in ['addressed_invites_recheck_issuer_recipient_role_and_never_restore_withdrawn_grants','competing_acceptances_respect_member_limit_and_expiry','team_hard_limit_applies_to_actual_member_jobs_and_never_copies_the_organization_wallet','existing_team_budget_capacity_is_reconstructed_on_upgrade_without_minting_credit']:
        if 'team_operations::'+case+' ... ok' not in result.stdout:raise SystemExit('Missing team operation case '+case)

    if browser:
        case='team_operations::real_owner_and_member_browsers_recover_an_invitation_and_scope_the_member_budget'
        r=subprocess.run([cargo,'test','--offline','--locked','-p','orange-control-plane','--features','integration-tests','--test','user_identity',case,'--','--ignored','--exact','--nocapture'],cwd=root,env=env,text=True,stdout=subprocess.PIPE)
        print(r.stdout,end='',flush=True);r.check_returncode()
        if case not in r.stdout or 'test result: ok. 1 passed; 0 failed' not in r.stdout:raise SystemExit('Browser team case did not run')
