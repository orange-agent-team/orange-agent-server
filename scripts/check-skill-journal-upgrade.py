#!/usr/bin/env python3
"""Upgrade a real schema-29 journal and reject old-host access without rewriting the legacy run."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import time

ROOT=Path(__file__).resolve().parents[1]
PRIOR="94cfd1fc3e27dd2fb43438757af6d6b863b6e98d"
env=os.environ.copy();env["CARGO_NET_OFFLINE"]="true"
target=Path(env.get("CARGO_TARGET_DIR",ROOT/"target")).resolve();env["CARGO_TARGET_DIR"]=str(target)
cargo=Path(subprocess.check_output([shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup","which","--toolchain","1.97.1","cargo"],cwd=ROOT,text=True).strip())
env["PATH"]=str(cargo.parent)+os.pathsep+env["PATH"]
def build(root,out):
    subprocess.run([cargo,"build","--locked","--offline","-p","orange-agent-runtime","--example","local-agent"],cwd=root,env=env,check=True)
    shutil.copy2(target/"debug/examples/local-agent",out)

def main():
    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix="orange-skills-migration-") as tmp:
        base=Path(tmp);source=base/"prior";source.mkdir()
        archive=base/"source.tar"
        subprocess.run(["git","archive","--format=tar","--output",archive,PRIOR],cwd=ROOT,check=True)
        subprocess.run(["tar","-xf",archive,"-C",source],check=True)
        old=base/"old-agent";new=base/"new-agent";build(source,old);build(ROOT,new)
        state=base/"state";workspace=base/"workspace";state.mkdir();workspace.mkdir()
        database=state/"journal.sqlite";file=state/"private.json"
        config={"run":{"owner":"migration-owner","run_id":"old-run","policy":"local-read","environment":"pending","model_version":"offline","max_steps":3,"max_invocations":2,"max_output_tokens":32,"deadline_unix":int(time.time())+600},
                "prompt":"An existing journal entry","grant_id":"workspace","workspace":str(workspace),"journal":str(database),
                "private_roots":[str(state)],"model_socket":str(state/"unused.sock"),"model_key_file":str(state/"unused.key"),"context_tokens":4096}
        def write():file.write_text(json.dumps(config))
        def run(exe,*args):
            result=subprocess.run([exe,file,*args],env={"PATH":"/usr/bin:/bin"},capture_output=True,text=True,check=True)
            return json.loads(result.stdout)
        def snapshot():
            with sqlite3.connect("file:"+str(database)+"?mode=ro",uri=True) as db:
                return db.execute("PRAGMA user_version").fetchone()[0],db.execute("SELECT config_digest,messages,phase FROM runtime_runs WHERE run_id='old-run'").fetchone()
        write();config["run"]["environment"]=run(old,"grant-info")["environment"];write()
        assert run(old,"start")["phase"]=="ready"
        version,before=snapshot();assert version==29 and before
        registry=run(new,"skills");assert len(registry)>=2 and not any(x["enabled"] for x in registry)
        assert snapshot()==(30,before) and run(new,"skill-binding") is None
        template=next(x for x in registry if x["pin"]["id"]=="orange.text-summary")
        config["skill"]=template["pin"];write()
        assert run(new,"enable-skill","0")["revision"]==1
        assert subprocess.run([new,file,"start"],capture_output=True).returncode!=0
        config["run"]["run_id"]="new-skill-run";write()
        assert run(new,"start")["phase"]=="ready" and run(new,"skill-binding")==template["pin"]
        del config["skill"];config["run"]["run_id"]="old-run";write()
        fingerprint=hashlib.sha256(database.read_bytes()).hexdigest()
        refused=subprocess.run([old,file,"status"],capture_output=True,text=True)
        assert refused.returncode!=0 and "Unsupported" in refused.stderr,(refused.returncode,refused.stderr)
        assert hashlib.sha256(database.read_bytes()).hexdigest()==fingerprint and snapshot()==(30,before)
        print("PASS genuine schema 29 -> 30, unchanged messages/config, no implicit template, old executable refused before mutation")
if __name__=="__main__":main()
