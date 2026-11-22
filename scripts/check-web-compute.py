#!/usr/bin/env python3
"""Actual model/browser path using an explicitly selected, independently built Web checkout."""
from acceptance_origins import reserve_origins
from owned_postgres import database
import json
import argparse
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('--web-repo',required=True,type=Path)
parser.add_argument('--node',default=shutil.which('node'))
parser.add_argument('--team-text',action='store_true',help='Exercise an assigned member budget and assert separate personal and team settlement')
parser.add_argument('--database-url-file',type=Path,default=ROOT/'.local/database-url',help='Loopback PostgreSQL instance for the isolated team qualification')
args=parser.parse_args()
port=int(os.environ.get('ORANGE_WEB_COMPUTE_QA_PORT','43127'))
if port not in (43127,43128):raise SystemExit('Private compute QA origin required')
reserve_origins(port)
web=args.web_repo.resolve()
driver=web/'apps/web/scripts/real-compute-driver.mjs'
if not driver.is_file() or not (web/'apps/web/dist/index.html').is_file():raise SystemExit('Explicit built Web compute checkout required')
if not args.node or subprocess.check_output([args.node,'--version'],text=True).strip()!='v22.23.2':raise SystemExit('Pinned Node22.23.2 required')
with socket.socket() as probe:
    if probe.connect_ex(('127.0.0.1',port))==0:raise SystemExit(f'Isolated browser port {port} occupied; do not replace an unknown process')
build_env={**os.environ,'PATH':str(Path(args.node).parent)+os.pathsep+os.environ.get('PATH','')}
subprocess.run([args.node,web/'scripts/check.mjs'],cwd=web,env=build_env,check=True)
subprocess.run([sys.executable,web/'scripts/local-tls.py'],cwd=web,check=True)
env=os.environ.copy()
env.pop('ORANGE_TEAM_TEXT_BROWSER',None)
if args.team_text:env['ORANGE_TEAM_TEXT_BROWSER']='1'
env.pop('ORANGE_WITHDRAW_TESTS',None)
env.pop('ORANGE_QUEUE_EXPIRY_TESTS',None)
for flag in ['ORANGE_REAL_WORKER_PROFILE','ORANGE_WORKER_HOST_TESTS','ORANGE_PRICED_WORKER_TESTS','ORANGE_WORKER_OUTPUT_TESTS','ORANGE_WORKER_RENEWAL_TESTS','ORANGE_CLEANUP_PROTOCOL_TESTS','ORANGE_CLEANUP_REAL_TESTS','ORANGE_CANCEL_PROTOCOL_TESTS','ORANGE_CANCEL_REAL_TESTS','ORANGE_JOB_QUERY_TESTS','ORANGE_WEB_COMPUTE_TESTS']:
    env.pop(flag,None)
evidence=Path(tempfile.mkdtemp(prefix='web-compute-',dir=ROOT/'.local'));evidence.chmod(0o700)
env['ORANGE_WEB_COMPUTE_EVIDENCE_DIR']=str(evidence)
print('Private generated-test evidence: '+str(evidence.relative_to(ROOT)),flush=True)
env.update(ORANGE_WEB_COMPUTE_TESTS='1',ORANGE_PRICED_WORKER_TESTS='1',ORANGE_WEB_NODE_BINARY=str(Path(args.node).resolve()),ORANGE_WEB_COMPUTE_DRIVER=str(driver))
command=[sys.executable,ROOT/'scripts/check-real-worker.py']
if args.team_text:
    with database(args.database_url_file,'orange_team_text_') as (_,url):
        env['ORANGE_TEST_DATABASE_URL']=url
        subprocess.run(command,cwd=ROOT,env=env,check=True)
    browser=json.loads((evidence/'browser-result.json').read_bytes())
    settlement=json.loads((evidence/'team-text-settlement.json').read_bytes())
    if not browser['team_budget_recovery'] or not settlement['member_personal_wallet_unchanged'] or settlement['job']!=browser['job_id'] or int(settlement['charged_microcredits'])<=0:
        raise RuntimeError('The real team budget and settlement were not verified')
    print(json.dumps(settlement),flush=True)
else:
    subprocess.run(command,cwd=ROOT,env=env,check=True)
print('PASS explicit Web checkout with actual owned API/Worker/Counter and Chrome; ephemeral session/price/enrollment fixtures, no payment or production certification')
