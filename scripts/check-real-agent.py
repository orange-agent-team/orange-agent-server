#!/usr/bin/env python3
"""Real pinned model -> explicit isolated-owner approval -> real file -> durable recovery.

No synthetic model replies and no auto-approval in product code. This acceptance
driver only approves bounded reads of the fixture it creates itself.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import selectors
import shutil
import sqlite3
import subprocess
import tempfile
import time
import uuid

ROOT=Path(__file__).resolve().parents[1]
PRIVATE=ROOT/'.local/model-runtime'
REPORT=ROOT/'.local/real-agent'
parser=argparse.ArgumentParser()
parser.add_argument('--required-tool',choices=['read_file','search_files','search_text','ask_user'],default='read_file')
parser.add_argument('--model-protocol',choices=['chat_completions','responses'],default='chat_completions')
parser.add_argument('--model-runtime-root',type=Path,default=PRIVATE)
parser.add_argument('--verify-skills',action='store_true',help='Also run a digest-pinned, tool-free text summary and pin the read-only question template')
parser.add_argument('--verify-local-package',action='store_true',help='Install a reviewed local template and execute it using the real CPU model')
args=parser.parse_args()
if args.verify_local_package: args.verify_skills=True
PRIVATE=args.model_runtime_root.resolve()
if args.model_protocol!='chat_completions':
    REPORT=ROOT/'.local'/('real-agent-'+args.model_protocol)
if args.verify_skills:REPORT=ROOT/'.local'/('real-agent-local-package' if args.verify_local_package else 'real-agent-skills')
target=Path(os.environ.get('CARGO_TARGET_DIR',ROOT/'target')).resolve()

def digest(path):
    value=hashlib.sha256()
    with path.open('rb') as stream:
        for data in iter(lambda:stream.read(1024*1024),b''):value.update(data)
    return value.hexdigest()

def write(path,value):
    descriptor=os.open(path,os.O_CREAT|os.O_WRONLY|os.O_NOFOLLOW,0o600)
    with os.fdopen(descriptor,'w') as stream:
        stream.truncate();json.dump(value,stream);stream.flush();os.fsync(stream.fileno())

def main():
    os.umask(0o077)
    test_run_id=uuid.uuid4().hex
    write(REPORT/'last-report.json',{'passed':False,'phase':'preparing','test_run_id':test_run_id})
    lock=json.loads((ROOT/'config/local-model.lock.json').read_text())
    build=json.loads((PRIVATE/'build.json').read_text())
    backend=PRIVATE/'build/bin/llama-server';model=PRIVATE/(lock['model']['sha256']+'.gguf')
    if not model.is_file() or model.stat().st_size!=lock['model']['bytes'] or digest(model)!=lock['model']['sha256']:raise RuntimeError('Pinned actual model required')
    if build['backend_commit']!=lock['backend']['commit'] or build['gpu_enabled'] or digest(backend)!=build['executable_sha256']:raise RuntimeError('Verified fixed CPU backend required')
    rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
    cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
    env=os.environ.copy();env['PATH']=str(cargo.parent)+os.pathsep+env.get('PATH','')
    subprocess.run([str(cargo),'build','-p','orange-agent-runtime','--example','local-agent','--locked'],cwd=ROOT,env=env,check=True)
    subprocess.run([str(cargo),'build','-p','orange-model-runtime','--bin','orange-model-host','--locked'],cwd=ROOT,env=env,check=True)
    REPORT.mkdir(parents=True,exist_ok=True,mode=0o700)
    result={'scope':'actual pinned CPU model, isolated local file, explicit test-owner approvals and SQLite recovery; not hosted Agent, public identity, charges or GPU',
            'model_sha256':lock['model']['sha256'],'backend_commit':lock['backend']['commit'],'phases':[],'approved_operations':0,'passed':False,'test_run_id':test_run_id,'model_protocol':args.model_protocol,'required_tool':args.required_tool,'observed_tools':[]}
    # Short private root avoids Unix socket path limits. Never use user-selected directories.
    with tempfile.TemporaryDirectory(prefix='oa-real-',dir='/private/tmp' if os.uname().sysname=='Darwin' else None) as temporary:
        base=Path(temporary).resolve();runtime=base/('durable-model-state-'*6);ipc=base/'m';state=base/'j';workspace=base/'w'
        for folder in [runtime,ipc,state,workspace]:folder.mkdir(mode=0o700)
        result['separate_private_ipc']=True
        result['durable_socket_path_bytes']=len(os.fsencode(runtime/'engine.sock'))
        result['actual_socket_path_bytes']=len(os.fsencode(ipc/'engine.sock'))
        if result['durable_socket_path_bytes']<=100 or result['actual_socket_path_bytes']>100:raise RuntimeError('Long-state/short-IPC regression not exercised')
        sentinel='ORANGE_EVIDENCE_'+uuid.uuid4().hex
        (workspace/'proof.txt').write_text(sentinel+'\n')
        nonce=uuid.uuid4().hex+uuid.uuid4().hex;instance=uuid.uuid4().hex
        write(runtime/'profile.json',{'protocol':1,'runtime_directory':str(runtime),'socket_directory':str(ipc),'executable':str(backend),'executable_sha256':build['executable_sha256'],
            'model':str(model),'model_sha256':lock['model']['sha256'],'model_bytes':lock['model']['bytes'],'model_version':'qwen-agent-test-v1','context_tokens':4096,
            'startup_timeout_seconds':120,'control_nonce':nonce,'instance_id':instance})
        with (runtime/'host.log').open('w') as log:
            host=subprocess.Popen([str(target/'debug/orange-model-host'),str(runtime/'profile.json')],cwd=ROOT,env={'PATH':'/usr/bin:/bin'},stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True)
            try:
                host.stdin.write('auth '+nonce+'\n');host.stdin.flush()
                selector=selectors.DefaultSelector();selector.register(host.stdout,selectors.EVENT_READ)
                for _ in range(30):
                    if host.poll() is not None:raise RuntimeError('Actual model host exited before ready')
                    host.stdin.write('ping\n');host.stdin.flush()
                    if selector.select(5):break
                else:raise RuntimeError('Actual model readiness deadline')
                ready=json.loads(host.stdout.readline());selector.close()
                if ready['record']['phase']!='ready' or ready['record']['instance_id']!=instance:raise RuntimeError('Wrong model instance')
                config={'run':{'owner':'real-agent-fixture-owner','run_id':uuid.uuid4().hex,'policy':'read-only-fixture-v1','environment':'pending-grant-info','model_version':'qwen-agent-test-v1',
                    'max_steps':12,'max_invocations':6,'max_output_tokens':256,'deadline_unix':int(time.time())+180},'grant_id':'real-agent-fixture-grant','workspace':str(workspace),
                    'journal':str(state/'journal.sqlite'),'private_roots':[str(runtime),str(ipc),str(state),str(PRIVATE)],'model_socket':str(ipc/'engine.sock'),'model_protocol':args.model_protocol,
                    'model_key_file':str(runtime/'api.key'),'context_tokens':4096,
                    'prompt':'Use read_file to read the existing workspace file proof.txt with the relative path proof.txt. Do not guess its content. After the tool result is returned, answer with the exact file text.'}
                if args.required_tool=='search_files':
                    config['prompt']='Perform these steps in order. First call search_files with exactly these arguments: {"path":".","glob":"proof.txt"}. Use the literal filename proof.txt as the glob, not a wildcard. Wait for its result. Then call read_file with exactly {"path":"proof.txt"}. Finally answer with the exact file text. Do not invent content or call read_file before receiving the search result.'
                elif args.required_tool=='search_text':
                    config['prompt']='Call search_text with exactly these arguments: {"path":".","query":"ORANGE_EVIDENCE_"}. Do not add optional fields. Do not call read_file. Wait for the search result. Then answer with the exact complete matching line text from its excerpt. Do not invent or shorten the content.'
                if args.required_tool=='ask_user':
                    config['user_questions']=True
                    config['prompt']='First call ask_user with exactly these arguments: {"questions":[{"prompt":"Which file should I read?"}]}. Wait for the user answer. Do not call any filesystem tool before that answer. After receiving the answers array, your next response must call read_file with path equal to the answered filename. Do not produce prose or merely repeat the filename. Only after receiving the read_file result may you return the exact file content. The task is not complete until the file content is returned.'
                path=state/'agent.json';write(path,config)
                def invoke(operation,*arguments,expect_error=False,input_json=None):
                    child=subprocess.Popen([str(target/'debug/examples/local-agent'),str(path),operation,*arguments],cwd=ROOT,env={'PATH':'/usr/bin:/bin'},stdin=subprocess.PIPE if input_json is not None else subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
                    try:
                        for _ in range(25):
                            try:out,error=child.communicate(input=json.dumps(input_json) if input_json is not None and _==0 else None,timeout=5);break
                            except subprocess.TimeoutExpired:host.stdin.write('ping\n');host.stdin.flush()
                        else:raise RuntimeError('Bounded CLI transition deadline')
                        if expect_error:
                            if child.returncode==0 or 'ApprovalRequired' not in error:raise RuntimeError('Unapproved tool was not explicitly rejected')
                            return None
                        if child.returncode:raise RuntimeError('Local Agent transition failed: '+error[:1000])
                        return json.loads(out)
                    finally:
                        if child.poll() is None:child.kill();child.wait()
                config['run']['environment']=invoke('grant-info')['environment'];write(path,config)
                if args.verify_skills:
                    if args.required_tool!='read_file':raise ValueError('Real Skills check uses the read-file fixture')
                    catalog={row['pin']['id']:row for row in invoke('skills')}
                    summary=catalog['orange.text-summary'];qa=catalog['orange.source-question']
                    if summary['tools'] or summary['enabled'] or set(qa['tools'])!={'read_file','search_text'}:raise ValueError('Unexpected bundled grants')
                    if args.verify_local_package:
                        manifest={'id':'local.delivery-report','version':'1.0.0','name':'Delivery summary','scenario':'AU-02',
                                  'instruction':'Summarize the delivery note in one sentence. Preserve all dates and quantities.','tools':[]}
                        review=invoke('review-skill',input_json=manifest)
                        if review['installed'] or review['template']!=manifest:raise ValueError('Review changed the document or installed it')
                        installed=invoke('install-skill',review['pin']['digest'],'0',input_json=manifest)
                        if installed['installation_revision']!=1:raise ValueError('Installation revision missing')
                        summary=next(s for s in invoke('skills') if s['pin']==review['pin'])
                        if summary['enabled']:raise ValueError('Install activated the package')
                    original=json.loads(json.dumps(config))
                    config['skill']=summary['pin'];config['run']['run_id']=uuid.uuid4().hex
                    config['prompt']='The delivery will arrive Friday. It contains 37 crates of oranges.'
                    write(path,config);activation=invoke('enable-skill','0')
                    if activation['revision']!=1:raise ValueError('Expected original activation')
                    if args.verify_local_package:
                        config['skill_activation_revision']=activation['revision'];write(path,config)
                    before=invoke('start')
                    if before['phase']!='ready' or invoke('skill-binding')!=summary['pin']:raise ValueError('Run lacks its original skill binding')
                    response=invoke('step')
                    answer=(response.get('final_text') or '').lower()
                    if response['phase']!='completed' or 'friday' not in answer or '37' not in answer:raise ValueError('Real summary lost the delivery day or quantity')
                    with sqlite3.connect(config['journal']) as db:
                        request=json.loads(db.execute('SELECT request FROM runtime_invocations WHERE run_id=?',(config['run']['run_id'],)).fetchone()[0])
                        if request['tools'] or request['messages'][0]['role']!='system' or request['messages'][-1]['content']!=config['prompt']:raise ValueError('Incorrect skill authority or separated input')
                    if invoke('disable-skill','1')['revision']!=2:raise ValueError('Disable did not advance activation')
                    if args.verify_local_package:
                        if invoke('uninstall-skill','1','2')['installation_revision']!=2:raise ValueError('Uninstall revision missing')
                        if invoke('skill-binding')!=summary['pin'] or any(s['pin']==summary['pin'] for s in invoke('skills')):raise ValueError('Uninstall lost history or left an installed version')
                        result['reviewed_local_package']={'pin':summary['pin'],'retained_historical_binding':True,'explicit_install_and_activation':True}
                    config=original;config['skill']=qa['pin'];write(path,config)
                    invoke('enable-skill','0')
                    result['skills']={'actual_summary':True,'summary_pin':summary['pin'],'qa_pin':qa['pin'],'summary_tools':0,'day_and_quantity_preserved':True}
                view=invoke('start');result['phases'].append(view['phase'])
                def committed_tool_results(tool):
                    database=sqlite3.connect(state/'journal.sqlite')
                    try:
                        database.execute('PRAGMA query_only=ON')
                        return [json.loads(row[0]) for row in database.execute('SELECT result_json FROM operations WHERE tool_version=? AND state=? AND result_json IS NOT NULL',(tool+'.v1','succeeded'))]
                    finally:database.close()
                seen=set();negative=False
                for _ in range(16):
                    if view['phase']=='completed':break
                    if view['phase']=='awaiting_user':
                        if args.required_tool!='ask_user' or result.get('user_answers',0):raise RuntimeError('Unexpected extra question group')
                        question=view['user_question']
                        if len(question['questions'])!=1 or view['pending']:raise RuntimeError('Mixed question/tool authority')
                        if invoke('step')['user_question']!=question:raise RuntimeError('Waiting question changed or redispatched')
                        invoke('approve',question['question_id'],expect_error=True)
                        answer={'question_id':question['question_id'],'argument_digest':question['argument_digest'],'answers':['proof.txt']}
                        view=invoke('answer',input_json=answer)
                        if view['phase']!='ready' or invoke('answer',input_json=answer)!=view:raise RuntimeError('Reply did not commit exactly once')
                        database=sqlite3.connect(state/'journal.sqlite')
                        try:
                            database.execute('PRAGMA query_only=ON')
                            if database.execute('SELECT count(*) FROM approvals').fetchone()[0]!=0:raise RuntimeError('User answer incorrectly granted authority')
                        finally:database.close()
                        result['user_answers']=1;result['answer_did_not_approve_tools']=True;result['observed_tools'].append('ask_user');result['phases'].append(view['phase']);continue
                    if view['phase'] not in ['ready','tools_pending']:raise RuntimeError('Agent entered non-executable phase: '+str(view))
                    pending=view.get('pending',[])
                    if pending:
                        if not negative:
                            invoke('step',expect_error=True);negative=True
                        for item in pending:
                            request=item['request'];tool=request['tool'];arguments=request['arguments']
                            if tool=='read_file':
                                if arguments.get('path')!='proof.txt' or args.required_tool=='search_text' or (args.required_tool=='ask_user' and result.get('user_answers')!=1):raise RuntimeError('Unexpected read target/tool; approval refused')
                                if args.required_tool=='search_files' and not committed_tool_results('search_files'):raise RuntimeError('Required filename search was bypassed')
                            elif tool=='list_directory' and args.required_tool!='ask_user':
                                if arguments.get('path','') not in ['','.']:raise RuntimeError('Unexpected directory; approval refused')
                            elif tool=='search_files' and args.required_tool=='search_files':
                                if arguments.get('path')!='.' or arguments.get('glob')!='proof.txt':raise RuntimeError('Unexpected filename search scope; approval refused')
                            elif tool=='search_text' and args.required_tool=='search_text':
                                if arguments.get('path')!='.' or arguments.get('query')!='ORANGE_EVIDENCE_' or arguments.get('regular_expression',False):raise RuntimeError('Unexpected text search scope; approval refused')
                            else:raise RuntimeError('Unexpected capability; approval refused')
                            if item['operation_id'] not in seen:
                                invoke('approve',item['operation_id']);seen.add(item['operation_id']);result['approved_operations']+=1;result['observed_tools'].append(tool)
                    view=invoke('step');result['phases'].append(view['phase'])
                result['final_phase']=view['phase'];result['sentinel_matched']=sentinel in (view.get('final_text') or '')
                if view['phase']!='completed' or not result['sentinel_matched'] or not negative or not seen or args.required_tool not in result['observed_tools']:raise RuntimeError('Real model/tool/file evidence did not complete; no substitute response used')
                if args.verify_skills:
                    references=view.get('source_references',[])
                    proof=workspace/'proof.txt'
                    if not any(v['path']=='proof.txt' and v['sha256']==digest(proof) and int(v['start_byte'])==0 and int(v['end_byte'])==proof.stat().st_size for v in references):
                        raise RuntimeError('Host did not bind the final answer to the actual authorized read')
                    result['skills']['verified_source_references']=len(references)
                if args.required_tool=='ask_user':
                    database=sqlite3.connect(state/'journal.sqlite')
                    try:
                        database.execute('PRAGMA query_only=ON')
                        replies=database.execute("SELECT answers FROM runtime_questions WHERE state='answered'").fetchall()
                        if len(replies)!=1 or json.loads(replies[0][0])!=['proof.txt']:raise RuntimeError('Missing original committed user answer')
                    finally:database.close()
                required_result='read_file' if args.required_tool=='ask_user' else args.required_tool
                committed=committed_tool_results(required_result)
                if not committed:raise RuntimeError('Required tool never committed an actual successful result')
                for value in committed:
                    if value.get('kind')!=required_result:raise RuntimeError('Wrong committed tool result type')
                    data=value['data']
                    if args.required_tool=='search_files' and (data['paths']!=['proof.txt'] or data['truncated']):raise RuntimeError('Filename search did not locate the exact fixture')
                    if args.required_tool=='search_text':
                        matches=data['matches']
                        expected=hashlib.sha256((sentinel+'\n').encode()).hexdigest()
                        if data['truncated'] or len(matches)!=1 or matches[0]['path']!='proof.txt' or matches[0]['sha256']!=expected or matches[0]['excerpt']!=sentinel:raise RuntimeError('Text search did not return the actual versioned fixture line')
                result['required_tool_result_verified']=True
                def evidence_counts():
                    db=sqlite3.connect(state/'journal.sqlite')
                    try:
                        db.execute('PRAGMA query_only=ON')
                        return {'model_invocations':db.execute('SELECT count(*) FROM runtime_invocations').fetchone()[0],
                                'operations':db.execute('SELECT count(*) FROM operations').fetchone()[0],
                                'committed_results':db.execute('SELECT count(*) FROM operations WHERE result_json IS NOT NULL').fetchone()[0]}
                    finally:db.close()
                before=evidence_counts();restored=invoke('show');again=invoke('step');after=evidence_counts()
                if restored!=view or again!=view or before!=after or before['committed_results']<1:raise RuntimeError('Completed recovery changed effects or lost committed evidence')
                result.update(before);result['preapproval_rejected']=True;result['reopen_no_replay']=True;result['passed']=True
            except Exception as error:
                result['failure']=str(error)[:1600]
                if (state/'journal.sqlite').is_file():
                    snapshot=REPORT/('failure-'+uuid.uuid4().hex)
                    try:
                        # CLI writers have exited. Preserve database and WAL
                        # together; diagnostics must not hide the original error.
                        shutil.copytree(state,snapshot,symlinks=True,ignore=shutil.ignore_patterns('agent.json'))
                        result['fixture_journal_snapshot']=str(snapshot.relative_to(ROOT))
                    except OSError as snapshot_error:
                        result['snapshot_error_kind']=type(snapshot_error).__name__
                raise
            finally:
                if host.stdin:host.stdin.close()
                host.wait(timeout=15)
                record_path=runtime/'process.json'
                result['model_stopped']=record_path.is_file() and json.loads(record_path.read_text())['phase']=='stopped'
                result['passed']=result['passed'] and result['model_stopped']
                write(REPORT/('report-'+test_run_id+'.json'),result)
                write(REPORT/'last-report.json',result)
                if not result['model_stopped']:raise RuntimeError('Actual model cleanup unconfirmed')
    print(json.dumps(result))

if __name__=='__main__':
    REPORT.mkdir(parents=True,exist_ok=True,mode=0o700)
    with (REPORT/'acceptance.lock').open('a') as guard:
        fcntl.flock(guard,fcntl.LOCK_EX|fcntl.LOCK_NB);main()
