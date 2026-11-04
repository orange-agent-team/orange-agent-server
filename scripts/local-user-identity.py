#!/usr/bin/env python3
"""Register a separate local user OIDC client in the task-owned Keycloak realm."""
import fcntl
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile
import urllib.request
import urllib.parse

ROOT=Path(__file__).resolve().parents[1]
PRIVATE=ROOT/'.local/identity'
BASE='http://127.0.0.1:44180'
ORIGIN='https://127.0.0.1:43122'
def private_json(path,value):
    fd,temp=tempfile.mkstemp(dir=path.parent,prefix='.identity-')
    try:
        with os.fdopen(fd,'w') as file:
            json.dump(value,file,indent=2);file.flush();os.fsync(file.fileno())
        os.replace(temp,path)
    finally:
        if os.path.exists(temp):os.unlink(temp)
def main():
    subprocess.run([sys.executable,str(ROOT/'scripts/local-identity.py'),'start'],check=True)
    with (PRIVATE/'user-client.lock').open('a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        fixture=json.loads((PRIVATE/'fixture.json').read_text())
        if 'user_client_secret' not in fixture:
            fixture['user_client_secret']=secrets.token_urlsafe(48)
            private_json(PRIVATE/'fixture.json',fixture)
        http=urllib.request.build_opener(urllib.request.ProxyHandler({}))
        body=urllib.parse.urlencode({'grant_type':'password','client_id':'admin-cli','username':'local-bootstrap','password':fixture['bootstrap_password']}).encode()
        request=urllib.request.Request(BASE+'/realms/master/protocol/openid-connect/token',data=body,headers={'Content-Type':'application/x-www-form-urlencoded'})
        with http.open(request,timeout=10) as response:token=json.load(response)['access_token']
        def call(method,path,value=None):
            payload=None if value is None else json.dumps(value).encode()
            request=urllib.request.Request(BASE+path,data=payload,method=method,headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
            with http.open(request,timeout=10) as response:
                raw=response.read()
                return json.loads(raw) if raw else None
        client={'clientId':'orange-user','enabled':True,'protocol':'openid-connect','publicClient':False,
                'secret':fixture['user_client_secret'],'standardFlowEnabled':True,'implicitFlowEnabled':False,
                'directAccessGrantsEnabled':False,'serviceAccountsEnabled':False,
                'redirectUris':[ORIGIN+'/api/v1/auth/callback'],'webOrigins':[ORIGIN],
                'attributes':{'pkce.code.challenge.method':'S256','acr.loa.map':'{"1":1,"2":2}'},
                'protocolMappers':[{'name':'verified-acr','protocol':'openid-connect','protocolMapper':'oidc-acr-mapper',
                                    'config':{'id.token.claim':'true','access.token.claim':'true'}}]}
        existing=call('GET','/admin/realms/orange-dev/clients?clientId=orange-user')
        if existing:
            if len(existing)!=1 or existing[0]['clientId']!='orange-user':raise RuntimeError('Unexpected local client identity')
            call('PUT','/admin/realms/orange-dev/clients/'+existing[0]['id'],client)
        else:call('POST','/admin/realms/orange-dev/clients',client)
        private_json(PRIVATE/'user-config.json',{'issuer':BASE+'/realms/orange-dev','client_id':'orange-user',
                    'client_secret':fixture['user_client_secret'],'user_origin':ORIGIN,'required_acr':'1','environment':'dev'})
    print('Separate local user OIDC client configured; standard password assurance, no platform role grants')
if __name__=='__main__':main()
