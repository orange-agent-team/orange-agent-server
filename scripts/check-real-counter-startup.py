#!/usr/bin/env python3
"""Start the actual local counter inside the User API and reject a final-byte model mismatch before launch."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import time

from owned_postgres import database
from platform_package_smoke import listening, request

ROOT=Path(__file__).resolve().parents[1]


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--environment",type=Path,default=ROOT/".local")
    args=cli.parse_args();private=args.environment.resolve()
    policy=json.loads((ROOT/"config/local-model.lock.json").read_bytes())["model"]
    rt=private/"model-runtime"
    build=json.loads((rt/"build.json").read_bytes())
    original=rt/(policy["sha256"]+".gguf")
    if original.stat().st_size!=policy["bytes"]:raise ValueError("Expected complete installed test model")
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
    env=dict(os.environ,CARGO_NET_OFFLINE="true",PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
    subprocess.run([cargo,"build","--offline","--locked","-p","orange-control-plane","--bin","orange-user-api"],cwd=ROOT,env=env,check=True)
    details=json.loads(subprocess.check_output([cargo,"metadata","--offline","--locked","--no-deps","--format-version","1"],cwd=ROOT,env=env))
    binary=Path(details["target_directory"])/"debug/orange-user-api"
    short=Path("/private/tmp") if platform.system()=="Darwin" else Path("/tmp")
    with database(private/"database-url","orange_counter_check_") as (_,url), tempfile.TemporaryDirectory(dir=short,prefix="oa-ctr-") as directory:
        root=Path(directory);objects=root/"store";objects.mkdir(mode=0o700)
        meter=root/"engine";meter.mkdir(mode=0o700)
        config=root/"counter.json";config.touch(mode=0o600)
        def configure(model,runtime):
            config.write_text(json.dumps({"protocol":1,"runtime_directory":str(runtime),"executable":str(rt/"build/bin/llama-server"),
                "executable_sha256":build["executable_sha256"],"model":str(model),"model_sha256":policy["sha256"],"model_bytes":policy["bytes"],
                "model_version":"model-test","context_tokens":4096}))
        app={"PATH":"/usr/bin:/bin","DATABASE_URL":url,"ORANGE_ENV":"development","ORANGE_USER_CONFIG_FILE":str(private/"identity/user-config.json"),
             "ORANGE_USER_BIND":"127.0.0.1:0","ORANGE_METER_CONFIG":str(config),"ORANGE_ARTIFACT_ROOT":str(objects),
             "ORANGE_ARTIFACT_BYTE_LIMIT":"1048576","ORANGE_ARTIFACT_RECORD_LIMIT":"100"}
        configure(original,meter)
        started=time.monotonic()
        with listening(binary,app,"User API") as origin:
            elapsed=time.monotonic()-started
            if elapsed>35 or request(origin,"/api/v1/session")[0]!=401:raise RuntimeError("Bounded identity-protected API not ready")
            owned=json.loads((meter/"process.json").read_bytes())
            if owned["phase"]!="ready" or owned["model_sha256"]!=policy["sha256"] or not owned["cpu_only"]:
                raise RuntimeError("Expected the owned pinned model before listening")
        if json.loads((meter/"process.json").read_bytes())["phase"]!="stopped":raise RuntimeError("Counter exit not confirmed")
        damaged=root/"modified.gguf"
        if platform.system()=="Darwin":
            subprocess.run(["cp","-c",original,damaged],check=True)
        else:
            shutil.copyfile(original,damaged)
        if os.path.samefile(damaged,original):raise ValueError("Independent fixture required")
        damaged.chmod(0o600)
        with damaged.open("r+b") as f:
            f.seek(-1,os.SEEK_END);value=f.read(1);f.seek(-1,os.SEEK_END);f.write(bytes([value[0]^1]))
        refused=root/"refused";refused.mkdir(mode=0o700);configure(damaged,refused)
        rejection=subprocess.run([binary],env=app,capture_output=True,timeout=35)
        if rejection.returncode==0 or b"listening on" in rejection.stderr or (refused/"process.json").exists():
            raise RuntimeError("Corrupted model was not refused before launch")
        print(json.dumps({"ready_seconds":round(elapsed,3),"unchanged_startup_budget":35,"model_bytes":policy["bytes"],
                          "real_counter_confirmed_stopped":True,"final_byte_mismatch_denied_before_launch":True}),flush=True)
    print("PASS authentic model counter startup and same-size changed-content refusal")


if __name__=="__main__":
    main()
