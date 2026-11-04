#!/usr/bin/env python3
"""Real loopback HTTP + PostgreSQL slice, with explicitly synthetic development funding/quote."""
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

root=Path(__file__).resolve().parents[1]
url=(root/'.local/database-url').read_text().strip()
parts=urllib.parse.urlsplit(url)
assert parts.hostname=='127.0.0.1' and parts.path=='/orange_dev', 'Refuse non-development DB'
environment=os.environ.copy()
environment.update(PGHOST=parts.hostname,PGPORT=str(parts.port),PGUSER=urllib.parse.unquote(parts.username),
                   PGPASSWORD=urllib.parse.unquote(parts.password),PGDATABASE='orange_dev')
psql='/opt/homebrew/opt/postgresql@17/bin/psql'
owner=str(uuid.uuid4());other=str(uuid.uuid4());token=secrets.token_urlsafe(36);other_token=secrets.token_urlsafe(36)
def sql(statement):
    subprocess.run([psql,'-v','ON_ERROR_STOP=1','-q'],input=statement,text=True,env=environment,check=True,capture_output=True)
sql("BEGIN; INSERT INTO owners(owner_id) VALUES ('"+owner+"'),('"+other+"'); INSERT INTO wallets(owner_id,gift) VALUES ('"+owner+"',10000),('"+other+"',10000); COMMIT;")
with socket.socket() as probe:
    probe.bind(('127.0.0.1',0));port=probe.getsockname()[1]
with tempfile.TemporaryDirectory(prefix='http-smoke-',dir=root/'.local') as folder:
    identity=Path(folder)/'identities.json';identity.write_text(json.dumps([{'owner_id':owner,'token':token},{'owner_id':other,'token':other_token}]))
    os.chmod(identity,0o600)
    child_env=os.environ.copy();child_env.update(ORANGE_ENV='development',ORANGE_BIND='127.0.0.1:'+str(port),DATABASE_URL=url,ORANGE_DEV_IDENTITIES_FILE=str(identity))
    log=(Path(folder)/'api.log').open('w')
    child=subprocess.Popen([str(root/'target/debug/orange-api')],env=child_env,cwd=root,stdout=log,stderr=log)
    base='http://127.0.0.1:'+str(port)
    def request(method,path,body=None,key=None,credential=token):
        headers={'Authorization':'Bearer '+credential}
        if key:headers['Idempotency-Key']=key
        data=None if body is None else json.dumps(body).encode()
        if data is not None:headers['Content-Type']='application/json'
        req=urllib.request.Request(base+path,data=data,method=method,headers=headers)
        try:
            with urllib.request.urlopen(req,timeout=5) as r:
                assert r.headers.get('Cache-Control')=='no-store'
                return r.status,json.load(r)
        except urllib.error.HTTPError as e:
            assert e.headers.get('Cache-Control')=='no-store'
            return e.code,json.load(e)
    try:
        ready=False
        for _ in range(60):
            if child.poll() is not None:raise RuntimeError('API exited before readiness; local private log retained until test cleanup')
            try:
                with urllib.request.urlopen(base+'/healthz',timeout=.3) as r:ready=r.status==200
            except OSError:pass
            if ready:break
            time.sleep(.1)
        assert ready,'API did not become ready'
        status,budget=request('POST','/api/v1/budgets',{'scope_id':'http-run','limit':'10000'},'create-root')
        assert status==200
        root_id=str(uuid.UUID(budget['root_budget_id']))
        assert request('POST','/api/v1/budgets',{'scope_id':'http-run','limit':'10000'},'create-root')[1]['root_budget_id']==root_id
        quote=str(uuid.uuid4());invocation=str(uuid.uuid4());digest='a'*64
        sql("INSERT INTO quotes(quote_id,owner_id,root_budget_id,invocation_id,projection_digest,price_version,max_amount,expires_at) VALUES ('"+quote+"','"+owner+"','"+root_id+"','"+invocation+"','"+digest+"','http-test-price',6000,clock_timestamp()+interval '5 minutes');")
        body={'root_budget_id':root_id,'invocation_id':invocation,'purpose':'primary','quote_id':quote,'request_projection_digest':digest}
        status,job=request('POST','/api/v1/jobs',body,'submit')
        assert status==200 and job['reserved']=='6000'
        assert request('POST','/api/v1/jobs',body,'submit')[1]['job_id']==job['job_id']
        assert request('GET','/api/v1/jobs/'+job['job_id'],credential=other_token)[0]==404
        assert request('GET','/api/v1/jobs/'+job['job_id'],credential='not-valid')[0]==401
        status,cancelled=request('POST','/api/v1/jobs/'+job['job_id']+'/cancel',key='cancel')
        assert status==200 and cancelled['execution_state']=='cancelled' and cancelled['charge']=='0'
        assert request('POST','/api/v1/jobs/'+job['job_id']+'/cancel',key='cancel')[1]['charge']=='0'
        assert request('GET','/api/v1/budgets/'+root_id)[1]['reserved']=='0'
        status,stream=request('GET','/api/v1/event-streams?'+urllib.parse.urlencode({'entity_kind':'job','entity_id':job['job_id']}))
        assert status==200
        stream_path='/api/v1/event-streams/'+stream['stream_id']
        status,first=request('GET',stream_path+'?after=0&limit=1')
        assert status==200 and [e['event_type'] for e in first['events']]==['job.queued']
        status,second=request('GET',stream_path+'?after='+first['next_cursor'])
        assert status==200 and [e['event_type'] for e in second['events']]==['job.cancelled']
        assert request('GET',stream_path+'?after=0',credential=other_token)[0]==404
        assert request('GET',stream_path+'?after=01')[0]==400
        print('PASS: actual HTTP + PostgreSQL create/replay/reserve/cross-owner reject/cancel/source release/event cursor replay/no-store')
        print('Identity/funding/quote are explicit development fixtures; no model execution, OIDC or payments certified')
    finally:
        if child.poll() is None:
            child.send_signal(signal.SIGINT)
            try:child.wait(timeout=5)
            except subprocess.TimeoutExpired:child.kill();child.wait(timeout=5)
        log.close()
