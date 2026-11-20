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
parser.add_argument('--web-repo',type=Path)
args=parser.parse_args()
browser={}
if args.web_repo:
    reserve_origins(43122)
    web=args.web_repo.resolve()
    node=Path.home()/'.nvm/versions/node/v22.23.2/bin/node'
    subprocess.run([node,web/'scripts/check.mjs'],cwd=web,check=True)
    import sys
    subprocess.run([sys.executable,web/'scripts/local-tls.py'],cwd=web,check=True)
    evidence=root/'.local/test-checkout-evidence';evidence.mkdir(parents=True,exist_ok=True,mode=0o700)
    browser=dict(ORANGE_CHECKOUT_BROWSER=str(web/'apps/web/scripts/real-test-checkout.mjs'),ORANGE_CHECKOUT_NODE=str(node),ORANGE_CHECKOUT_SCREENSHOT=str(evidence/'checked-order.png'))

rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','cargo'],cwd=root,text=True).strip())
protoc=os.environ.get('PROTOC') or shutil.which('protoc') or '/opt/homebrew/bin/protoc'
with database(args.database_url_file,'orange_checkout_') as (_,url):
    env={k:v for k,v in os.environ.items() if not k.startswith(('ORANGE_','STRIPE_'))}
    env.update(**browser,ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve()),PATH=str(cargo.parent)+os.pathsep+env['PATH'],PROTOC=protoc,CARGO_NET_OFFLINE='true')
    result=subprocess.run([cargo,'test','--locked','--offline','-p','orange-control-plane','--features','integration-tests','--test','test_checkout'],cwd=root,env=env,check=False,text=True,stdout=subprocess.PIPE)
    print(result.stdout,end='')
    result.check_returncode()
    if 'checkout_unknown_concurrency_raw_signatures_and_authoritative_test_credit_fulfillment ... ok' not in result.stdout:
        raise SystemExit('Required checkout case did not run')
    print('PASS real HTTP/PG and loopback merchant protocol; no actual Stripe account, external card, live charge or refund was used.')
