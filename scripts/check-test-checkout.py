#!/usr/bin/env python3
"""Real PostgreSQL and HTTP checkout flow using an isolated loopback Stripe wire fixture."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
from owned_postgres import database
from acceptance_origins import reserve_origins

root=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('--database-url-file',type=Path,default=root/'.local/database-url')
parser.add_argument('--identity-config',type=Path,default=root/'.local/identity/user-config.json')
parser.add_argument('--admin-identity-config',type=Path)
parser.add_argument('--web-repo',type=Path)
parser.add_argument('--admin-repo',type=Path)
args=parser.parse_args()
browser={}
ports=[port for repo,port in [(args.admin_repo,43112),(args.web_repo,43122)] if repo]
if ports: reserve_origins(*ports)
for repo,kind,driver,filename in [(args.web_repo,'CHECKOUT','real-test-checkout.mjs','checked-order.png'),(args.admin_repo,'REFUND_ADMIN','real-test-refund.mjs','approved-refund.png')]:
    if not repo:continue
    import sys
    repo=repo.resolve();node=Path.home()/'.nvm/versions/node/v22.23.2/bin/node'
    subprocess.run([node,repo/'scripts/check.mjs'],cwd=repo,check=True)
    subprocess.run([sys.executable,repo/'scripts/local-tls.py'],cwd=repo,check=True)
    evidence=root/'.local/test-checkout-evidence';evidence.mkdir(parents=True,exist_ok=True,mode=0o700)
    app='admin' if kind=='REFUND_ADMIN' else 'web'
    browser['ORANGE_'+kind+'_BROWSER']=str(repo/'apps'/app/'scripts'/driver)
    browser['ORANGE_'+kind+'_NODE']=str(node)
    browser['ORANGE_'+kind+'_SCREENSHOT']=str(evidence/filename)
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','cargo'],cwd=root,text=True).strip())
protoc=os.environ.get('PROTOC') or shutil.which('protoc') or '/opt/homebrew/bin/protoc'
with database(args.database_url_file,'orange_checkout_') as (_,url), database(args.database_url_file,'orange_checkout_old_') as (_,old):
    env={k:v for k,v in os.environ.items() if not k.startswith(('ORANGE_','STRIPE_'))}
    env.update(**browser,ORANGE_TEST_DATABASE_URL=url,ORANGE_CHECKOUT_OLD_DATABASE=old,ORANGE_TEST_OIDC_CONFIG_FILE=str((args.admin_identity_config or args.identity_config.with_name('admin-config.json')).resolve()),ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve()),PATH=str(cargo.parent)+os.pathsep+env['PATH'],PROTOC=protoc,CARGO_NET_OFFLINE='true')
    result=subprocess.run([cargo,'test','--locked','--offline','-p','orange-control-plane','--features','integration-tests','--test','test_checkout'],cwd=root,env=env,check=False,text=True,stdout=subprocess.PIPE)
    print(result.stdout,end='')
    result.check_returncode()
    if not all(name+' ... ok' in result.stdout for name in ['checkout_unknown_concurrency_raw_signatures_and_authoritative_test_credit_fulfillment','schema_58_purchase_and_job_history_survives_lot_upgrade_and_exact_queue_release']):
        raise SystemExit('Required checkout case did not run')
    print('PASS real HTTP/PG and loopback merchant protocol; no actual Stripe account, external card, live charge or refund was used.')
