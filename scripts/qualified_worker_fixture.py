"""Build the pinned foreground Worker and check its existing local base model."""
import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path

def prepare(root, private, text_runtime, env):
    cargo=Path(subprocess.check_output([shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup","which","--toolchain","1.97.1","cargo"],text=True).strip())
    env["PATH"]=str(cargo.parent)+os.pathsep+env["PATH"]
    subprocess.run([cargo,"build","--locked","--offline","-p","orange-worker","--bin","orange-worker"],cwd=root,env=env,check=True)
    target=json.loads(subprocess.check_output([cargo,"metadata","--locked","--offline","--no-deps","--format-version","1"],cwd=root,env=env))["target_directory"]
    env["ORANGE_OBSERVATION_WORKER"]=str(Path(target)/"debug/orange-worker")
    runtime=text_runtime.resolve();text_pin=json.loads((root/"config/local-model.lock.json").read_bytes())
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
