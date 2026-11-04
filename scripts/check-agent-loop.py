#!/usr/bin/env python3
"""Real SQLite/filesystem host recovery with explicitly scripted model responses."""
import os
import json
import tempfile
import time
from pathlib import Path
import shutil
import subprocess
ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
environment=os.environ.copy();environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','')
subprocess.run([str(cargo),'test','-p','orange-agent-runtime','--locked'],cwd=ROOT,env=environment,check=True)
subprocess.run([str(cargo),'test','-p','orange-protocol','model::tests','--locked'],cwd=ROOT,env=environment,check=True)
subprocess.run([str(cargo),'test','-p','orange-model-runtime','llama::tests','--locked'],cwd=ROOT,env=environment,check=True)
subprocess.run([str(cargo),'build','-p','orange-agent-runtime','--example','local-agent','--locked'],cwd=ROOT,env=environment,check=True)
with tempfile.TemporaryDirectory(prefix='orange-agent-cli-') as directory:
    base=Path(directory);private=base/'private';workspace=base/'workspace'
    private.mkdir(mode=0o700);workspace.mkdir()
    key=private/'model-key';key.write_text('development-cli-not-a-real-model-key');key.chmod(0o600)
    path=private/'config.json'
    config={'run':{'owner':'cli-user','run_id':'cli-run','policy':'cli-policy','environment':'pending','model_version':'cli-local-profile','max_steps':10,'max_invocations':5,'max_output_tokens':128,'deadline_unix':int(time.time())+120},'grant_id':'cli-grant','workspace':str(workspace),'journal':str(private/'journal.sqlite'),'private_roots':[str(private)],'model_endpoint':'http://127.0.0.1:44099/','model_key_file':str(key),'context_tokens':2048,'prompt':'Inspect an explicitly authorized workspace'}
    def save():
        path.write_text(json.dumps(config));path.chmod(0o600)
    def invoke(command):
        return json.loads(subprocess.check_output([str(ROOT/'target/debug/examples/local-agent'),str(path),command],cwd=ROOT,env=environment,text=True))
    save();config['run']['environment']=invoke('grant-info')['environment'];save()
    assert invoke('start')['phase']=='ready'
    assert invoke('show')['invocations']==0
    assert invoke('cancel')['phase']=='cancelled'
    assert invoke('show')['final_text'] is None
    assert invoke('start')['phase']=='cancelled', 'reopening same config must not reactivate revoked Run'
print('PASS actual local CLI grant/start/show/cancel; no model request sent by this CLI smoke')
print('PASS durable host/approval/recovery on actual SQLite and files; model transport is scripted in tests, real local inference remains separately gated')
