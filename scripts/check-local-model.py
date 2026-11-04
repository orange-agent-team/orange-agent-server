#!/usr/bin/env python3
"""Real, free CPU inference smoke. Owns and stops only the engine process it launches."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import subprocess
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
PRIVATE = ROOT/'.local/model-runtime'
def sha256(path):
    h=hashlib.sha256()
    with path.open('rb') as file:
        for block in iter(lambda:file.read(1024*1024), b''): h.update(block)
    return h.hexdigest()
def main():
    manifest=json.loads((ROOT/'config/local-model.lock.json').read_text())
    report=json.loads((PRIVATE/'build.json').read_text())
    binary=PRIVATE/'build/bin/llama-server'
    model=PRIVATE/(manifest['model']['sha256']+'.gguf')
    if report['backend_commit']!=manifest['backend']['commit'] or report['gpu_enabled'] is not False:
        raise SystemExit('Pinned CPU build required')
    if sha256(binary)!=report['executable_sha256'] or sha256(model)!=manifest['model']['sha256']:
        raise SystemExit('Executable/model verification failed; nothing launched')
    with socket.socket() as port:
        if port.connect_ex(('127.0.0.1',43104))==0:raise SystemExit('Dedicated test port occupied')
    rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
    cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
    environment=os.environ.copy();environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','')
    subprocess.run([str(cargo),'build','--bin','model-smoke','--locked'],cwd=ROOT,env=environment,check=True)
    key=secrets.token_urlsafe(32)
    for name,value in [('api-key',key),('client.json',json.dumps({'endpoint':'http://127.0.0.1:43104/','api_key':key}))]:
        with open(PRIVATE/name,'w',opener=lambda p,f:os.open(p,f,0o600)) as file:file.write(value)
    clean={'PATH':'/usr/bin:/bin','LANG':'en_US.UTF-8','TMPDIR':str(PRIVATE)}
    with open(PRIVATE/'engine.log','w',opener=lambda p,f:os.open(p,f,0o600)) as log:
        process=subprocess.Popen([str(binary),'--model',str(model),'--alias','qwen-local-test-v1',
            '--host','127.0.0.1','--port','43104','--api-key-file',str(PRIVATE/'api-key'),
            '--ctx-size','2048','--parallel','1','--threads','2','--threads-http','2',
            '--n-gpu-layers','0','--no-ui','--no-ui-mcp-proxy','--jinja','--no-context-shift'],
            cwd=PRIVATE,env=clean,stdout=log,stderr=log)
        try:
            for _ in range(120):
                if process.poll() is not None:raise RuntimeError('Engine exited; inspect private engine.log')
                try:
                    req=urllib.request.Request('http://127.0.0.1:43104/health',headers={'Authorization':'Bearer '+key})
                    with urllib.request.urlopen(req,timeout=1) as response:
                        if response.status==200:break
                except OSError:pass
                time.sleep(.5)
            else:raise RuntimeError('Engine readiness deadline exceeded')
            environment['ORANGE_LOCAL_MODEL_CONFIG']=str(PRIVATE/'client.json')
            subprocess.run([str(ROOT/'target/debug/model-smoke')],cwd=ROOT,env=environment,timeout=150,check=True)
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:process.wait(timeout=10)
                except subprocess.TimeoutExpired:process.kill();process.wait()
            print('Owned CPU engine exited; no persistent server or GPU release claim')
    print('PASS actual pinned CPU model adapter smoke; not full Agent, billing, GPU or platform isolation certification')
if __name__=='__main__':
    PRIVATE.mkdir(parents=True,exist_ok=True,mode=0o700)
    with (PRIVATE/'smoke.lock').open('a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        main()
