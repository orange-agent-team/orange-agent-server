#!/usr/bin/env python3
"""Exercise authenticated consent and real mTLS/PG ranged input on a disposable database."""
from locked_transcription_env import interpreter
import argparse
import json
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from owned_postgres import database
from qualified_worker_fixture import prepare
from acceptance_origins import reserve_origins

ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
for name, default in (("database-url-file",".local/database-url"),("identity-config",".local/identity/user-config.json"),
                      ("model-cache",".local/transcription-model")):
    parser.add_argument("--"+name,type=Path,default=ROOT/default)
parser.add_argument("--python",type=Path)
parser.add_argument("--text-runtime",type=Path,default=ROOT/".local/model-runtime")
parser.add_argument("--web-repo",type=Path)
parser.add_argument("--uv-cache",type=Path,default=ROOT/".local/uv-cache")
parser.add_argument("--six-journeys-only",action="store_true")
parser.add_argument("--provider-earnings",action="store_true")
parser.add_argument("--earning-admin",type=Path)
parser.add_argument("--market-metrics",action="store_true",help="Reconcile a pseudonymous reporting cohort with the actual two-account payment and CPU job receipts")
args=parser.parse_args()
if args.market_metrics and not args.six_journeys_only:parser.error("The market report needs the six-journey specimen")
if (args.provider_earnings or args.earning_admin) and not args.six_journeys_only:parser.error("Earning review requires the six-journey fixture")
args.python = args.python or interpreter(ROOT,args.uv_cache)
browser={}
if args.web_repo:
    reserve_origins(43122)
    web=args.web_repo.resolve();node=Path.home()/".nvm/versions/node/v22.23.2/bin/node"
    subprocess.run([node,web/"scripts/check.mjs"],cwd=web,check=True)
    subprocess.run([sys.executable,web/"scripts/local-tls.py"],cwd=web,check=True)
    evidence=ROOT/".local/audio-consent-evidence";evidence.mkdir(mode=0o700,parents=True,exist_ok=True)
    browser={"ORANGE_AUDIO_CONSENT_BROWSER":str(web/"apps/web/scripts/real-audio-consent.mjs"),"ORANGE_AUDIO_CONSENT_NODE":str(node),"ORANGE_AUDIO_CONSENT_SCREENSHOT":str(evidence/"authorization.png")}

if args.web_repo and (args.provider_earnings or args.earning_admin):
    browser["ORANGE_EARN_USER_BROWSER"]=str(web/"apps/web/scripts/real-provider-income.mjs")
    browser["ORANGE_EARN_USER_CAPTURE"]=str(evidence/"provider-income.png")
if args.earning_admin:
    reserve_origins(43112)
    admin=args.earning_admin.resolve();node=Path.home()/".nvm/versions/node/v22.23.2/bin/node"
    metadata=json.loads((admin/"docs/contracts/snapshot.json").read_bytes())
    for val in metadata["files"].values():
        if hashlib.sha256((ROOT/val["source"]).read_bytes()).hexdigest()!=val["sha256"]:raise ValueError("Earning review Admin contract mismatch")
    subprocess.run([node,admin/"scripts/check.mjs"],cwd=admin,check=True)
    subprocess.run([sys.executable,admin/"scripts/local-tls.py"],cwd=admin,check=True)
    capture=ROOT/".local/earning-review";capture.mkdir(parents=True,mode=0o700,exist_ok=True)
    browser.update(ORANGE_EARN_BROWSER=str(admin/"apps/admin/scripts/real-earning-review.mjs"),ORANGE_EARN_NODE=str(node),
                   ORANGE_EARN_SCREENSHOT=str(capture/"release.png"))
if args.provider_earnings or args.earning_admin:browser["ORANGE_PROVIDER_EARNING_TEST"]="1"

with database(args.database_url_file,"orange_audio_grant_") as (_,url), database(args.database_url_file,"orange_audio_old_") as (_,old), tempfile.TemporaryDirectory(prefix="orange-audio-grant-") as tmp:
    folder=Path(tmp);slot=folder/"slot";slot.mkdir(mode=0o700)
    config=folder/"probe.json";config.write_text(json.dumps({"python":str(args.python.absolute()),"model_cache":str(args.model_cache.resolve()),"private_slot":str(slot)}));config.chmod(0o600)
    env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
    env.update(**browser,ORANGE_TEST_DATABASE_URL=url,ORANGE_AUDIO_OLD_DATABASE_URL=old,ORANGE_AUDIO_GRANT_TESTS="1",ORANGE_ASR_PROBE_FILE=str(config),
               ORANGE_TEST_OIDC_CONFIG_FILE=str(args.identity_config.with_name("admin-config.json").resolve()),ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve()),ORANGE_AUDIO_SAMPLE=str(args.model_cache.resolve()/"jfk.wav"),CARGO_NET_OFFLINE="true")
    prepare(ROOT,folder,args.text_runtime,env)
    if args.market_metrics:
        evidence=Path(tempfile.mkdtemp(prefix="market-metrics-",dir=ROOT/".local"));evidence.chmod(0o700)
        env["ORANGE_MARKET_REPORT_EVIDENCE"]=str(evidence)
        print("Private market report evidence:",evidence.relative_to(ROOT),flush=True)
    if args.six_journeys_only:env["ORANGE_SIX_JOURNEY_TEST"]="1"
    subprocess.run([sys.executable,ROOT/"scripts/check-worker-session.py"],cwd=ROOT,env=env,check=True)

if args.market_metrics:
    report=json.loads((evidence/"paid-cohort.report.json").read_bytes())
    if report["microcredits"]["charged"]!="4400" or report["currency_totals"][0]["net_receipts_minor"]!="900":
        raise RuntimeError("Unconfirmed original merchant and settlement observations")
    print("PASS marketplace SQL cohort and redacted report",flush=True)
