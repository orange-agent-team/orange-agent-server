#!/usr/bin/env python3
"""Real CLI/SQLite/filesystem/HTTP protocol with an explicit scripted model.

No real inference, paid model, user directory or approval authority is simulated
as production certification. Answers never authorize the subsequent read.
"""
import http.server
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import threading
import time

ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
env=os.environ.copy();env['PATH']=str(cargo.parent)+os.pathsep+env.get('PATH','')
for command in [['test','-p','orange-agent-runtime','user_question','--locked'],['test','-p','orange-protocol','questions::tests','--locked'],['build','-p','orange-agent-runtime','--example','local-agent','--locked']]:
    subprocess.run([str(cargo),*command],cwd=ROOT,env=env,check=True,timeout=240)
metadata=json.loads(subprocess.check_output([str(cargo),'metadata','--no-deps','--format-version','1','--locked'],cwd=ROOT,env=env,text=True))
binary=Path(metadata['target_directory'])/'debug/examples/local-agent'
if env.get('CARGO_BUILD_TARGET'):raise SystemExit('CLI acceptance requires a native build')
observations=[];failures=[]
class ModelFixture(http.server.BaseHTTPRequestHandler):
    def log_message(self,*args):pass
    def do_POST(self):
        try:
            self.connection.settimeout(5)
            length=int(self.headers.get('Content-Length','0'))
            assert 0<length<=256*1024
            assert self.headers.get('Authorization')=='Bearer QuestionModelFixtureKeyForLocalProtocolOnly0000000000000000000000'
            request=json.loads(self.rfile.read(length))
            if self.path=='/v1/chat/completions/input_tokens':
                payload=b'{"input_tokens":1}'
                self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(payload)));self.end_headers();self.wfile.write(payload);return
            assert self.path=='/v1/chat/completions'
            assert any(t['function']['name']=='ask_user' for t in request['tools'])
            index=len(observations);observations.append(request)
            if index==0:
                delta={'role':'assistant','tool_calls':[{'index':0,'id':'clarify-1','type':'function','function':{'name':'ask_user','arguments':json.dumps({'questions':[{'prompt':'Which file?','choices':['note.txt','other.txt']}]})}}]};finish='tool_calls'
            elif index==1:
                answers=[m for m in request['messages'] if m.get('tool_call_id')=='clarify-1']
                assert len(answers)==1 and json.loads(answers[0]['content'])=={'answers':['note.txt']}
                delta={'role':'assistant','tool_calls':[{'index':0,'id':'read-1','type':'function','function':{'name':'read_file','arguments':'{"path":"note.txt"}'}}]};finish='tool_calls'
            elif index==2:
                assert any(m.get('tool_call_id')=='read-1' and 'question-cli-proof' in m['content'] for m in request['messages'])
                delta={'role':'assistant','content':'fixture-complete'};finish='stop'
            else:raise AssertionError('unexpected model replay')
            rid='fixture-response-'+str(index)
            events=[{'id':rid,'choices':[{'index':0,'delta':delta,'finish_reason':None}]},{'id':rid,'choices':[{'index':0,'delta':{},'finish_reason':finish}]}]
            payload=(''.join('data: '+json.dumps(e)+'\n\n' for e in events)+'data: [DONE]\n\n').encode()
            self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(payload)));self.end_headers();self.wfile.write(payload)
        except Exception as error:
            failures.append(type(error).__name__)
            self.send_error(400,'explicit model fixture refused request')

server=http.server.ThreadingHTTPServer(('127.0.0.1',0),ModelFixture)
thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
try:
    with tempfile.TemporaryDirectory(prefix='orange-user-questions-') as directory:
        base=Path(directory);private=base/'private';workspace=base/'workspace';private.mkdir(mode=0o700);workspace.mkdir()
        (workspace/'note.txt').write_text('question-cli-proof')
        key=private/'key';key.write_text('QuestionModelFixtureKeyForLocalProtocolOnly0000000000000000000000');key.chmod(0o600)
        config_path=private/'config.json';database=private/'journal.sqlite'
        config={'run':{'owner':'question-cli-owner','run_id':'question-cli-run','policy':'readonly-fixture','environment':'pending','model_version':'question-fixture','max_steps':12,'max_invocations':6,'max_output_tokens':128,'deadline_unix':int(time.time())+120},'grant_id':'question-grant','workspace':str(workspace),'journal':str(database),'private_roots':[str(private)],'model_endpoint':'http://127.0.0.1:'+str(server.server_port)+'/', 'model_key_file':str(key),'context_tokens':2048,'prompt':'Clarify the file, then read after separate approval','user_questions':True}
        def save():config_path.write_text(json.dumps(config));config_path.chmod(0o600)
        def invoke(command,*arguments,data=None,failed=False):
            result=subprocess.run([str(binary),str(config_path),command,*arguments],cwd=ROOT,env=env,input=json.dumps(data) if data is not None else None,capture_output=True,text=True,timeout=20)
            if failed:
                assert result.returncode!=0,'invalid call was accepted';return None
            if result.returncode:raise RuntimeError('CLI '+command+' failed: '+result.stderr[:500])
            return json.loads(result.stdout)
        save();config['run']['environment']=invoke('grant-info')['environment'];save()
        assert invoke('start')['phase']=='ready'
        pending=invoke('step');assert pending['phase']=='awaiting_user' and pending['pending']==[]
        assert invoke('show')['user_question']==pending['user_question']
        assert invoke('step')['phase']=='awaiting_user' and len(observations)==1
        question=pending['user_question'];answer={'question_id':question['question_id'],'argument_digest':question['argument_digest'],'answers':['note.txt']}
        invoke('approve',question['question_id'],failed=True)
        invoke('answer','inline-user-text',data=answer,failed=True)
        invoke('answer',data={**answer,'grant_access':True},failed=True)
        ready=invoke('answer',data=answer);assert ready['phase']=='ready'
        assert invoke('answer',data=answer)['steps']==ready['steps']
        invoke('answer',data={**answer,'answers':['different']},failed=True)
        pending=invoke('step');assert pending['phase']=='tools_pending'
        invoke('step',failed=True)
        with sqlite3.connect(database) as db:
            assert db.execute('SELECT count(*) FROM approvals').fetchone()[0]==0
            assert db.execute("SELECT count(*) FROM runtime_questions WHERE state='answered'").fetchone()[0]==1
        invoke('approve',pending['pending'][0]['operation_id'])
        assert invoke('step')['phase']=='ready'
        complete=invoke('step');assert complete['phase']=='completed' and complete['final_text']=='fixture-complete'
        assert invoke('show')==complete and len(observations)==3
        (workspace/'note.txt').unlink();invoke('show',failed=True);invoke('step',failed=True)
        assert len(observations)==3 and not failures
finally:
    server.shutdown();server.server_close();thread.join(timeout=5)
print('PASS actual CLI/stdin, current source reads, immutable questions/answers, restart/idempotency and separate tool approval. Model/count/identity are explicit local fixtures; no real inference or production identity certification')
