#!/usr/bin/env python3
"""Real mTLS, reviewed-profile fixture and actual CPU sample on a disposable PostgreSQL instance."""
import argparse
import json
import hashlib
import shutil
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
with database(args.database_url_file,"orange_asr_peer_") as (_,url),tempfile.TemporaryDirectory(prefix="orange-asr-peer-") as tmp:
    private=Path(tmp);slot=private/"slot";slot.mkdir(mode=0o700)
    cfg=private/"probe.json";cfg.write_text(json.dumps({"python":str(args.python.absolute()),"model_cache":str(args.model_cache.resolve()),"private_slot":str(slot)}));cfg.chmod(0o600)
    env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
    env.update(ORANGE_TRANSCRIPTION_OBSERVATION_TESTS="1",ORANGE_TEST_DATABASE_URL=url,ORANGE_ASR_PROBE_FILE=str(cfg),CARGO_NET_OFFLINE="true")
    cargo=Path(subprocess.check_output([shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup","which","--toolchain","1.97.1","cargo"],text=True).strip())
    env["PATH"]=str(cargo.parent)+os.pathsep+env["PATH"]
    subprocess.run([cargo,"build","--locked","--offline","-p","orange-worker","--bin","orange-worker"],cwd=root,env=env,check=True)
    target=json.loads(subprocess.check_output([cargo,"metadata","--locked","--offline","--no-deps","--format-version","1"],cwd=root,env=env))["target_directory"]
    env["ORANGE_OBSERVATION_WORKER"]=str(Path(target)/"debug/orange-worker")
    runtime=args.text_runtime.resolve();text_pin=json.loads((root/"config/local-model.lock.json").read_bytes())
    build=json.loads((runtime/"build.json").read_bytes());exe=runtime/"build/bin/llama-server";model=runtime/(text_pin["model"]["sha256"]+".gguf")
    def sha(path):
        h=hashlib.sha256()
        with path.open("rb") as f:
            for b in iter(lambda:f.read(1024*1024),b""):h.update(b)
        return h.hexdigest()
    if (build["gpu_enabled"] or build["backend_commit"]!=text_pin["backend"]["commit"] or sha(exe)!=build["executable_sha256"]
        or model.stat().st_size!=text_pin["model"]["bytes"] or sha(model)!=text_pin["model"]["sha256"]):
        raise ValueError("Existing approved text host base required")
    text_profile={"executable":str(exe),"executable_sha256":build["executable_sha256"],"model":str(model),"model_sha256":text_pin["model"]["sha256"],
                  "model_bytes":model.stat().st_size,"model_version":"model-test","context_tokens":4096,"startup_timeout_seconds":120}
    text_file=private/"text-profile.json";text_file.write_text(json.dumps(text_profile));text_file.chmod(0o600)
    env["ORANGE_OBSERVATION_TEXT_PROFILE"]=str(text_file)
    subprocess.run([sys.executable,root/"scripts/check-worker-session.py"],cwd=root,env=env,check=True)
