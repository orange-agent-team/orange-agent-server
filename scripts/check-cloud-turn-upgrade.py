#!/usr/bin/env python3
"""Rehearse schema 40 -> 41 with real historical release data in an owned DB."""
import argparse
import hashlib
import json
import os
from pathlib import Path,PurePosixPath
import shutil
import subprocess
import sys
import tarfile
import tempfile
from urllib.parse import unquote,urlsplit
from owned_postgres import database
from platform_test_pki import prepare

root=Path(__file__).resolve().parents[1]
server_base='c99526e05da1e1d9d4b8350e6460460fe03313ba'
web_base='db6a549c07a76a4fc0aaaa8368e78cad6a1e3325'
parser=argparse.ArgumentParser()
parser.add_argument('--database-url-file',required=True,type=Path)
parser.add_argument('--identity-config',required=True,type=Path)
parser.add_argument('--web-repo',required=True,type=Path)
parser.add_argument('--model-runtime-root',type=Path,default=root/'.local/model-runtime')
parser.add_argument('--archive',type=Path,default=root/'.local/gvisor-download/gvisor-aarch64.tar.bz2')
parser.add_argument('--npm-cache',type=Path)
parser.add_argument('--node',type=Path,default=shutil.which('node'))
args=parser.parse_args()
if args.npm_cache is not None:args.npm_cache=args.npm_cache.resolve(strict=True)
identity=json.loads(args.identity_config.read_text())
if identity.get('environment')!='dev' or urlsplit(identity['issuer']).hostname not in ('localhost','127.0.0.1','::1'):
    raise SystemExit('Local development issuer required')
node=args.node.resolve()
if subprocess.check_output([node,'--version'],text=True).strip()!='v22.23.2':raise SystemExit('Node 22.23.2 required')
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())

def extract(repo,revision,target):
    with tempfile.TemporaryFile() as archive:
        subprocess.run(['git','-C',repo,'archive','--format=tar',revision],stdout=archive,check=True)
        archive.seek(0)
        with tarfile.open(fileobj=archive) as t:
            for m in t:
                path=PurePosixPath(m.name)
                if path.is_absolute() or '..' in path.parts or not (m.isdir() or m.isfile()):raise SystemExit('Unexpected historical source archive entry')
                destination=target.joinpath(*path.parts)
                if m.isdir():destination.mkdir(parents=True,exist_ok=True)
                else:
                    destination.parent.mkdir(parents=True,exist_ok=True)
                    with t.extractfile(m) as source, destination.open('xb') as out:shutil.copyfileobj(source,out)
                    destination.chmod(0o755 if m.mode & 0o111 else 0o644)

def unchanged_facts(url):
    parts=urlsplit(url)
    pg=dict(os.environ,PGHOST=parts.hostname,PGPORT=str(parts.port or 5432),PGUSER=unquote(parts.username or ''),PGPASSWORD=unquote(parts.password or ''),
            PGDATABASE=parts.path[1:],PGCONNECT_TIMEOUT='5',PGOPTIONS='-c timezone=UTC')
    psql=shutil.which('psql') or '/opt/homebrew/opt/postgresql@17/bin/psql'
    tables=('cloud_runs cloud_workspaces cloud_run_events cloud_model_plans cloud_model_proposals cloud_model_quotes cloud_model_jobs cloud_model_results '
            'cloud_tool_responses cloud_operation_intents cloud_operation_decisions cloud_dispatch_permits cloud_operation_results '
            'cloud_directory_revocations artifact_tombstones job_response_commits job_settlements ledger_transactions ledger_entries wallets budget_roots').split()
    inventory={}
    for table in tables:
        rows=subprocess.check_output([psql,'-X','-w','-At','-v','ON_ERROR_STOP=1','-c',f'SELECT to_jsonb(t)::text FROM {table} t ORDER BY to_jsonb(t)::text'],env=pg)
        inventory[table]=hashlib.sha256(rows).hexdigest()
    return inventory

os.umask(0o077)
with tempfile.TemporaryDirectory(prefix='orange-upgrade-') as folder, database(args.database_url_file,'orange_cloud_model_') as (name,url):
    private=Path(folder);old=private/'server';web=private/'web';tls=private/'tls';tls.mkdir()
    extract(root,server_base,old);extract(args.web_repo.resolve(),web_base,web)
    expected=json.loads((old/'config/cloud-isolation.lock.json').read_text())
    archive=args.archive.resolve()
    if archive.stat().st_size!=expected['size_bytes'] or hashlib.sha256(archive.read_bytes()).hexdigest()!=expected['sha256']:raise SystemExit('Pinned gVisor required')
    local=old/'.local/gvisor-download';local.mkdir(parents=True)
    try:os.link(archive,local/expected['archive'])
    except OSError:shutil.copyfile(archive,local/expected['archive'])
    evidence=root/'.local'/'turn-upgrade'/name;evidence.mkdir(mode=0o700,parents=True)
    prepare(tls);db_file=tls/'database-url';db_file.write_text(url+'\n');db_file.chmod(0o600)
    env=dict(os.environ,PATH=str(node.parent)+os.pathsep+str(cargo.parent)+os.pathsep+os.environ['PATH'],CARGO_NET_OFFLINE='true',CARGO_INCREMENTAL='0',
             CARGO_PROFILE_DEV_DEBUG='0',CARGO_PROFILE_TEST_DEBUG='0',PYTHONDONTWRITEBYTECODE='1',
             ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve()),ORANGE_TEST_MODEL_RUNTIME_ROOT=str(args.model_runtime_root.resolve()),
             ORANGE_CLOUD_MODEL_BROWSER=str(web/'apps/web/scripts/real-cloud-model.mjs'),ORANGE_CLOUD_MODEL_QA_DIR=str(tls),
             ORANGE_CLOUD_DIRECTORY_TOOLS='1',ORANGE_CLOUD_MODEL_EVIDENCE=str(evidence),ORANGE_CLOUD_QA_PYTHON=sys.executable)
    cache=args.npm_cache or args.web_repo.resolve()/'.local/npm-cache'
    subprocess.run([node,node.parent/'../lib/node_modules/npm/bin/npm-cli.js','ci','--offline','--cache',cache,'--no-audit','--no-fund'],cwd=web/'apps/web',env=env,check=True)
    subprocess.run([node,web/'scripts/check.mjs'],cwd=web,env=env,check=True)
    # The old binaries are used only to produce the durable historical facts. Keep
    # their compiler cache out of the second build's disk peak, while retaining the
    # source checkouts, database, original receipts and fixtures for comparison.
    with tempfile.TemporaryDirectory(prefix="historical-target-",dir=private) as first_target:
        env["CARGO_TARGET_DIR"]=first_target
        subprocess.run([sys.executable,old/'scripts/check-web-compute.py','--web-repo',web,'--node',node],env=env,cwd=old,check=True)
        scratch_kib=int(subprocess.check_output(["du","-sk",first_target],text=True).split()[0])
    if Path(first_target).exists() or scratch_kib<=0:
        raise RuntimeError("Historical compiler cache was not released before migration")
    print("Released historical compiler output before the new build:",scratch_kib,"KiB",flush=True)
    original=json.loads((evidence/'cloud-model-result.json').read_text())
    if not original['original_operation_receipts_verified'] or len(original['directory_runs'])!=3:raise SystemExit('Incomplete prior release evidence')
    before=unchanged_facts(url)
    env.update(CARGO_TARGET_DIR=str(private/'current-migration-target'),ORANGE_CLOUD_UPGRADE_DATABASE=name,ORANGE_CLOUD_UPGRADE_REPORT=str(evidence/'upgrade.json'))
    subprocess.run([cargo,'test','--locked','--offline','-p','orange-control-plane','--features','cloud-upgrade-tests','--test','cloud_turn_upgrade'],
                   env=env,cwd=root,check=True)
    after=unchanged_facts(url)
    if before!=after:raise SystemExit('Historical records changed across migration')
    report=json.loads((evidence/'upgrade.json').read_text());report.update(previous_server=server_base,previous_web=web_base,historical_table_sha256=before,historical_compiler_cache_kib=scratch_kib,historical_compiler_cache_released_before_migration=True)
    (evidence/'upgrade.json').write_text(json.dumps(report,indent=2)+'\n')
    print('PASS actual historical records, matching input projections, settlements, withdrawals and unknown facts across schema upgrade: '+str(evidence.relative_to(root)))
