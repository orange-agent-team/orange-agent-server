#!/usr/bin/env python3
"""Drive the normal local CLI against owned Unix socket faults and check exact durable observations."""
import json
import os
from pathlib import Path
from http.server import BaseHTTPRequestHandler
import shutil
import sqlite3
import socketserver
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
class FaultHost(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True

class Response(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get('Content-Length', '0')))
        self.server.calls += 1
        self.server.received.set()
        behavior=self.server.behavior
        status=503
        content_type='application/json'
        data=b''
        if behavior in ('protocol','limit','incomplete'):
            status=200
            if behavior=='protocol':data=b'{}'
            elif behavior=='limit':data=b'{"input_tokens":1000000}'
            elif self.path.endswith('/input_tokens'):data=b'{"input_tokens":4}'
            else:
                content_type='text/event-stream'
                data=b'data: [DONE]\n\n'
        elif behavior!='error':
            self.server.release.wait(15)
        try:
            self.send_response(status)
            self.send_header('Content-Length',str(len(data)))
            self.send_header('Content-Type',content_type)
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError,ConnectionResetError):
            pass

    def log_message(self,*args):
        pass

def check(exe,base,behavior):
    base.mkdir()
    workspace=base/'w';state=base/'s'
    workspace.mkdir();state.mkdir()
    socket=state/'engine.sock';key=state/'api.key';key.write_text('fixturekey0123456789'*3)
    with FaultHost(str(socket),Response) as server:
        server.calls=0;server.behavior=behavior
        server.received=threading.Event();server.release=threading.Event()
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        cfg=state/'private.json'
        run={'owner':'fixture-owner','environment':'pending','run_id':'private-run','policy':'read-only','model_version':'local-fault-fixture',
             'max_steps':4,'max_invocations':3,'max_output_tokens':32,'deadline_unix':int(time.time())+(4 if behavior=='deadline' else 600)}
        conf={'run':run,'workspace':str(workspace),'journal':str(state/'journal.sqlite'),'private_roots':[str(state)],'grant_id':'workspace',
              'prompt':'Private synthetic dispatch fixture','context_tokens':4096,'model_socket':str(socket),'model_key_file':str(key)}
        def invoke(command):
            result=subprocess.run([exe,cfg,command],capture_output=True,text=True,check=True,timeout=20)
            return json.loads(result.stdout)
        child=None
        try:
            cfg.write_text(json.dumps(conf))
            run['environment']=invoke('grant-info')['environment'];cfg.write_text(json.dumps(conf))
            assert invoke('start')['phase']=='ready' and invoke('model-observation') is None
            if behavior=='interrupted':
                child=subprocess.Popen([exe,cfg,'step'],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
                assert server.received.wait(10),'The original model dispatch did not reach the owned endpoint'
                child.terminate();child.wait(timeout=10)
            else:
                assert invoke('step')['phase']=='recovery_required'
            observation=invoke('model-observation')
            assert observation['kind']=={'interrupted':'host_reopened','deadline':'run_deadline'}.get(behavior,'gateway_failed'),observation
            if behavior in ('error','protocol','limit','incomplete'):assert observation.get('adapter_code')=={'error':'unavailable'}.get(behavior,behavior),observation
            else: assert 'adapter_code' not in observation
            if behavior=='interrupted':
                assert 'elapsed_ms' not in observation and 'wait_limit_ms' not in observation
            else:
                elapsed=int(observation['elapsed_ms']);limit=int(observation['wait_limit_ms'])
                assert 0<limit<=120000
                if behavior=='deadline':assert 0<limit<=4000 and elapsed>=limit
                else:assert limit==120000
            assert invoke('step')['phase']=='recovery_required' and invoke('model-observation')==observation
            assert server.calls==(2 if behavior=='incomplete' else 1)
            with sqlite3.connect(state/'journal.sqlite') as db:
                assert db.execute('SELECT count(*) FROM runtime_invocations').fetchone()[0]==1
            assert set(observation)<= {'invocation','kind','observed_at_unix','wait_limit_ms','elapsed_ms','adapter_code'}
            print(json.dumps({'case':behavior,'observation':observation,'actual_endpoint_calls':server.calls}),flush=True)
        finally:
            if child is not None and child.poll() is None:child.kill();child.wait(timeout=10)
            server.release.set();server.shutdown();thread.join(timeout=5)

def main():
    os.umask(0o077)
    rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
    cargo=Path(subprocess.check_output([rustup,'which','--toolchain','1.97.1','cargo'],cwd=ROOT,text=True).strip())
    env=dict(os.environ,CARGO_NET_OFFLINE='true',PATH=str(cargo.parent)+os.pathsep+os.environ['PATH'])
    target=Path(env.get('CARGO_TARGET_DIR',ROOT/'target')).resolve();env['CARGO_TARGET_DIR']=str(target)
    subprocess.run([cargo,'build','--locked','--offline','-p','orange-agent-runtime','--example','local-agent'],cwd=ROOT,env=env,check=True)
    with tempfile.TemporaryDirectory(prefix='omd-',dir='/private/tmp' if os.uname().sysname=='Darwin' else None) as tmp:
        base=Path(tmp).resolve();exe=base/'agent';shutil.copy2(target/'debug/examples/local-agent',exe)
        for case in ('error','protocol','limit','incomplete','deadline','interrupted'):check(exe,base/case,case)
    print('PASS real CLI / SQLite / private HTTP faults, original dispatch recovery and no replay')

if __name__=='__main__':main()
