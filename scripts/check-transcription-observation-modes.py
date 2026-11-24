#!/usr/bin/env python3.12
"""Verify location-only observation without executing critical packages, then compare normal import resolution."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

ROOT=Path(__file__).resolve().parents[1]

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--uv-cache",type=Path,default=ROOT/".local/uv-cache")
    args=parser.parse_args()
    pin=(ROOT/"config/transcription-probe.v1.json").read_bytes()
    critical=json.loads(pin)["critical_modules"]
    roots=sorted({name.partition(".")[0] for name in critical})
    source=(ROOT/"crates/model-runtime/src/transcription_probe.py").read_text()
    guard=f"""
import sys
forbidden=frozenset({roots!r})
def audit(event,args):
    if event=="import" and args[0].partition(".")[0] in forbidden:
        raise RuntimeError("File observation tried to execute a measured package")
sys.addaudithook(audit)
"""
    with tempfile.TemporaryDirectory(prefix="orange-asr-files-") as tmp:
        install=Path(tmp)/"locked"
        subprocess.run(["uv","sync","--locked","--offline","--no-editable","--extra","transcription","--python","3.12","--project",ROOT/"backends"],
            cwd=ROOT,env=dict(os.environ,UV_CACHE_DIR=str(args.uv_cache.resolve()),UV_PROJECT_ENVIRONMENT=str(install),UV_LINK_MODE="copy"),check=True)
        observed=[]
        seconds={}
        for mode in ("files","imported"):
            start=time.monotonic()
            cmd=[install/"bin/python","-I","-B","-c",(guard if mode=="files" else "")+source,mode]
            raw=subprocess.check_output(cmd,input=pin,env={"HF_HUB_OFFLINE":"1"},timeout=120)
            seconds[mode]=round(time.monotonic()-start,3)
            observed.append(json.loads(raw))
        if observed[0]!=observed[1] or sorted(observed[0]["modules"])!=sorted(critical):
            raise ValueError("Static module identity differs from actual import resolution")
        print(json.dumps({"same_runtime_observations":True,"packages":len(observed[0]["packages"]),"critical_modules":len(critical),
            "measured_packages_imported_during_file_scan":0,"elapsed_seconds":seconds}))
        print("PASS cold ASR file measurements execute no measured package and match import-based verification")
if __name__=="__main__":main()
