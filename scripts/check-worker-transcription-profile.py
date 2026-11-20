#!/usr/bin/env python3
"""Qualify a fixed CPU ASR execution profile at the actual local Worker entry point."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument("--python",type=Path,default=ROOT/".local/transcription-venv/bin/python")
parser.add_argument("--model-cache",type=Path,default=ROOT/".local/transcription-model")
parser.add_argument("--uv-cache",type=Path,default=ROOT/".local/uv-cache")
args=parser.parse_args()
if not __debug__:raise SystemExit("Assertions required")
cargo=Path(subprocess.check_output([shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup",
                                    "which","--toolchain","1.97.1","cargo"],text=True).strip())
env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
subprocess.run([cargo,"build","--locked","--offline","-p","orange-worker","--bin","orange-worker"],cwd=ROOT,env=env,check=True)
binary=Path(os.environ.get("CARGO_TARGET_DIR",ROOT/"target")).resolve()/"debug/orange-worker"
pin=json.loads((ROOT/"config/local-transcription.lock.json").read_bytes())
env_pin=json.loads((ROOT/"config/transcription-probe.v1.json").read_bytes())
with tempfile.TemporaryDirectory(prefix="orange-asr-worker-") as temp:
    directory=Path(temp);slot=directory/"slot";slot.mkdir(mode=0o700)
    cfg=directory/"private-config.json"
    def perform(python,cache,expected=None,denied=False,mode=0o600,extra=None):
        spec={"python":str(python.absolute()),"model_cache":str(cache.resolve()),"private_slot":str(slot)}
        if expected is not None:spec["expected_execution_profile_digest"]=expected
        if extra:spec.update(extra)
        cfg.write_text(json.dumps(spec));cfg.chmod(mode)
        ret=subprocess.run([binary,"probe-transcription",cfg],env={"PATH":"/usr/bin:/bin"},text=True,
                           capture_output=True,timeout=120)
        if denied:
            assert ret.returncode!=0 and not ret.stdout, "Unqualified runtime returned a usable receipt"
            return
        if ret.returncode:raise RuntimeError("Worker probe failed: "+ret.stderr[:160])
        receipt=json.loads(ret.stdout);profile=receipt["profile"]
        canonical=json.dumps(profile,sort_keys=True,ensure_ascii=False,separators=(",",":")).encode()
        digest=hashlib.sha256(b"orange-request-v1\0worker.transcription-profile.v1\0"+canonical).hexdigest()
        assert receipt["execution_profile_digest"]==digest
        assert receipt["sample_sha256"]==pin["qualification_sample"]["sha256"]
        assert receipt["normalized_samples"]==176000 and receipt["sample_duration_ms"]==11000
        assert profile["runtime"]["packages"]==env_pin["packages"] and profile["device"]=="cpu"
        assert profile["model_sha256"]==pin["files"]["model.bin"]["sha256"]
        assert profile["max_input_bytes"]==100000000 and profile["max_duration_ms"]==600000
        assert len(profile["runtime"]["modules"])==len(env_pin["critical_modules"]) and not any(k in ret.stdout for k in [str(ROOT),str(directory),"http://","https://"])
        return receipt
    first=perform(args.python,args.model_cache)
    admitted=first["execution_profile_digest"]
    env2=directory/"new-venv"
    subprocess.run(["uv","sync","--locked","--offline","--no-editable","--extra","transcription","--python","3.12","--project",ROOT/"backends"],
                   cwd=ROOT,env=dict(env,UV_CACHE_DIR=str(args.uv_cache.resolve()),UV_PROJECT_ENVIRONMENT=str(env2),UV_LINK_MODE="copy"),check=True)
    other=env2/"bin/python";second=perform(other,args.model_cache,admitted)
    assert second==first,"Private venv paths changed the portable execution identity"
    perform(other,args.model_cache,"0"*64,denied=True)
    perform(other,args.model_cache,denied=True,mode=0o644)
    perform(other,args.model_cache,denied=True,extra={"remote_url":"https://invalid.example"})
    corrupted=directory/"corrupted";corrupted.mkdir(mode=0o700)
    sample=bytearray((args.model_cache/"jfk.wav").read_bytes());sample[-1]^=1
    (corrupted/"jfk.wav").write_bytes(sample)
    perform(other,corrupted,denied=True)
    module=Path(subprocess.check_output([other,"-I","-c","import importlib.util;print(importlib.util.find_spec('orange_inference_backends.embedding').origin)"],text=True).strip())
    assert module.resolve().is_relative_to(env2.resolve()) and not module.is_symlink()
    replacement=module.with_name("modified.py")
    replacement.write_bytes(module.read_bytes()+b"\n# changed qualification fixture\n");os.replace(replacement,module)
    perform(other,args.model_cache,denied=True)
    evidence=ROOT/".local/transcription-evidence";evidence.mkdir(mode=0o700,parents=True,exist_ok=True)
    record={"receipt":first,"two_separate_installs":True,"changed_helper_rejected":True,"changed_sample_rejected":True,
            "wrong_execution_profile_rejected":True,"private_config_enforced":True,"remote_registration":False}
    (evidence/"worker-profile.json").write_text(json.dumps(record,indent=2)+"\n")
    print(json.dumps({"profile":admitted,"packages":len(env_pin["packages"]),"critical_modules":len(env_pin["critical_modules"]),
                      "real_cpu_samples":176000,"path_independent":True,"mutations_rejected":True}))
