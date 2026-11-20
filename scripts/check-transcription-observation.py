#!/usr/bin/env python3
"""Real mTLS, reviewed-profile fixture and actual CPU sample on a disposable PostgreSQL instance."""
import argparse
import json
from qualified_worker_fixture import prepare
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from owned_postgres import database

root=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument("--database-url-file",type=Path,default=root/".local/database-url")
parser.add_argument("--python",type=Path,default=root/".local/transcription-venv/bin/python")
parser.add_argument("--model-cache",type=Path,default=root/".local/transcription-model")
parser.add_argument("--text-runtime",type=Path,default=root/".local/model-runtime")
args=parser.parse_args()
with database(args.database_url_file,"orange_asr_peer_") as (_,url), database(args.database_url_file,"orange_asr_old_") as (_,old), tempfile.TemporaryDirectory(prefix="orange-asr-peer-") as tmp:
    private=Path(tmp);slot=private/"slot";slot.mkdir(mode=0o700)
    cfg=private/"probe.json";cfg.write_text(json.dumps({"python":str(args.python.absolute()),"model_cache":str(args.model_cache.resolve()),"private_slot":str(slot)}));cfg.chmod(0o600)
    env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
    env.update(ORANGE_TRANSCRIPTION_OBSERVATION_TESTS="1",ORANGE_TEST_DATABASE_URL=url,ORANGE_ASR_UPGRADE_DATABASE_URL=old,ORANGE_ASR_PROBE_FILE=str(cfg),CARGO_NET_OFFLINE="true")
    prepare(root,private,args.text_runtime,env)
    subprocess.run([sys.executable,root/"scripts/check-worker-session.py"],cwd=root,env=env,check=True)
