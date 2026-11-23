#!/usr/bin/env python3
"""Workspace-only Keycloak fixture. Credentials are random/private, never production identities.

Uses the dedicated Colima socket; no default Docker context, host mounts or paid services.
Sources: Keycloak 26.8.1 containers guide and ConditionalLoaAuthenticator source.
"""
import argparse
import io
import json
import os
from pathlib import Path
import secrets
import subprocess
import tarfile
import time
import tempfile
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
PRIVATE = ROOT / '.local/identity'
IMAGE = 'quay.io/keycloak/keycloak@sha256:3f3c5f8e03c614774b4bd9173b88391fefa4153bfc010d8012d59ad825377332'
from development_profile import IDENTITY_BASE, IDENTITY_NAME as NAME, IDENTITY_PORT, INSTANCE
ISSUER = IDENTITY_BASE+'/realms/orange-dev'
ORIGIN = 'https://127.0.0.1:43112'
DOCKER = ['docker', '--host', 'unix://' + str(Path.home()/'.colima/orange-agent/docker.sock')]

def private_json(name, value):
    path = PRIVATE/name
    with tempfile.NamedTemporaryFile(mode="w",dir=PRIVATE,delete=False) as file:
        try:
            json.dump(value,file,indent=2)
            file.flush()
            os.fsync(file.fileno())
            os.replace(file.name,path)
        finally:
            Path(file.name).unlink(missing_ok=True)
    return path

def fixture():
    path = PRIVATE/'fixture.json'
    if path.exists():
        return json.loads(path.read_text())
    value = {'client_secret': secrets.token_urlsafe(48), 'bootstrap_password': secrets.token_urlsafe(32),
             'users': [{'username': 'orange-'+name, 'subject': str(uuid.uuid4()),
                        'password': secrets.token_urlsafe(32), 'otp': secrets.token_hex(16),
                        'principal_id': 'local-admin-'+name, 'role': role}
                       for name, role in [('operator', 'compute_operator'), ('auditor', 'auditor')]]}
    private_json('fixture.json', value)
    return value

def realm(value):
    def execution(authenticator, priority, config=None):
        result = {'authenticator': authenticator, 'requirement': 'REQUIRED', 'priority': priority,
                  'authenticatorFlow': False, 'userSetupAllowed': False}
        if config:
            result['authenticatorConfig'] = config
        return result
    flows = [{'alias': 'orange-mfa', 'providerId': 'basic-flow', 'topLevel': True, 'builtIn': False,
              'authenticationExecutions': [
                  {'flowAlias': 'orange-loa-'+str(i), 'authenticatorFlow': True,
                   'requirement': 'CONDITIONAL', 'priority': i*10, 'userSetupAllowed': False}
                  for i in [1, 2]]}]
    for level, auth in [(1, 'auth-username-password-form'), (2, 'auth-otp-form')]:
        flows.append({'alias': 'orange-loa-'+str(level), 'providerId': 'basic-flow',
                      'topLevel': False, 'builtIn': False, 'authenticationExecutions': [
                          execution('conditional-level-of-authentication', 10, 'orange-level-'+str(level)),
                          execution(auth, 20)]})
    return {'realm': 'orange-dev', 'enabled': True, 'sslRequired': 'none',
            'registrationAllowed': False, 'resetPasswordAllowed': False, 'rememberMe': False,
            'loginWithEmailAllowed': False, 'accessTokenLifespan': 900,
            'browserFlow': 'orange-mfa', 'authenticationFlows': flows,
            'authenticatorConfig': [{'alias': 'orange-level-'+str(i), 'config':
                                     {'loa-condition-level': str(i), 'loa-max-age': '0'}} for i in [1, 2]],
            'otpPolicyType': 'totp', 'otpPolicyAlgorithm': 'HmacSHA1', 'otpPolicyDigits': 6,
            'otpPolicyPeriod': 30, 'otpPolicyLookAheadWindow': 1,
            'clients': [{'clientId': 'orange-admin', 'enabled': True, 'protocol': 'openid-connect',
                         'publicClient': False, 'secret': value['client_secret'],
                         'standardFlowEnabled': True, 'directAccessGrantsEnabled': False,
                         'serviceAccountsEnabled': False, 'redirectUris': [ORIGIN+'/admin/v1/auth/callback'],
                         'webOrigins': [ORIGIN], 'attributes': {'pkce.code.challenge.method': 'S256',
                                                              'acr.loa.map': '{"1":1,"2":2}'},
                         'protocolMappers': [{'name': 'verified-acr', 'protocol': 'openid-connect',
                                              'protocolMapper': 'oidc-acr-mapper',
                                              'config': {'id.token.claim': 'true', 'access.token.claim': 'true'}}]}],
            'users': [{'id': u['subject'], 'username': u['username'], 'enabled': True,
                       'emailVerified': True, 'firstName': 'Local', 'lastName': 'Fixture',
                       'email': u['username']+'@example.invalid', 'credentials': [
                           {'type': 'password', 'value': u['password'], 'temporary': False},
                           {'type': 'otp', 'userLabel': 'local-test-only',
                            'secretData': json.dumps({'value': u['otp']}),
                            'credentialData': json.dumps({'subType': 'totp', 'digits': 6,
                                                         'counter': 0, 'period': 30, 'algorithm': 'HmacSHA1'})}]}
                      for u in value['users']]}

def owned_container():
    result=subprocess.check_output(DOCKER+["ps","-aq","--no-trunc","--filter","name=^/"+NAME+"$"],text=True).strip()
    if not result:
        return None
    info=json.loads(subprocess.check_output(DOCKER+["inspect",result]))[0]
    cfg=info["Config"]
    binding=info["HostConfig"]["PortBindings"].get("8080/tcp")
    expected={"HostIp":"127.0.0.1","HostPort":str(IDENTITY_PORT)}
    proof=PRIVATE/"fixture.json"
    if (info["Name"]!="/"+NAME or cfg["Labels"].get("org.orange-agent.purpose")!="local-identity" or
        cfg["Image"]!=IMAGE or binding!=[expected] or not proof.is_file() or proof.is_symlink()):
        raise SystemExit("Container identity or owning fixture mismatch")
    secret=json.loads(proof.read_bytes())["bootstrap_password"]
    if not any(secrets.compare_digest(v,"KC_BOOTSTRAP_ADMIN_PASSWORD="+secret) for v in cfg["Env"]):
        raise SystemExit("Container is owned by a different development fixture")
    return info


def main():
    action = argparse.ArgumentParser()
    action.add_argument('action', choices=['start', 'stop', 'status', 'remove'])
    args = action.parse_args()
    PRIVATE.mkdir(parents=True, exist_ok=True, mode=0o700)
    existing=owned_container()
    if args.action in ("stop","remove"):
        if args.action=="remove" and INSTANCE=="default":
            raise SystemExit("Removal requires an explicitly named development fixture")
        if existing:
            command=["stop",existing["Id"]] if args.action=="stop" else ["rm","-f",existing["Id"]]
            subprocess.run(DOCKER+command,check=True,stdout=subprocess.DEVNULL)
        print("Dedicated identity fixture "+args.action+" complete")
        return
    if args.action=="status":
        print("running" if existing and existing["State"]["Running"] else "not running")
        return
    value = fixture()
    private_json('admin-config.json', {'issuer': ISSUER, 'client_id': 'orange-admin',
                                     'client_secret': value['client_secret'], 'admin_origin': ORIGIN,
                                     'required_acr': '2', 'environment': 'dev'})
    if not existing:
        envfile = PRIVATE/'container.env'
        with open(envfile, 'w', opener=lambda p, f: os.open(p, f, 0o600)) as file:
            file.write('KC_BOOTSTRAP_ADMIN_USERNAME=local-bootstrap\nKC_BOOTSTRAP_ADMIN_PASSWORD='+value['bootstrap_password']+'\n')
        subprocess.run(DOCKER+['create', '--name', NAME, '--label', 'org.orange-agent.purpose=local-identity',
                              '--cpus', '1', '--memory', '1536m', '--pids-limit', '512',
                              '--publish', f"127.0.0.1:{IDENTITY_PORT}:8080", '--env-file', str(envfile),
                              IMAGE, 'start-dev', '--import-realm', '--hostname', IDENTITY_BASE],
                       check=True, stdout=subprocess.DEVNULL)
        existing=owned_container()
    if existing["State"]["Status"]=="created":
        # Copy a private archive into the dedicated container; never mount the workspace.
        payload = json.dumps(realm(value)).encode()
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode='w') as tar:
            entry = tarfile.TarInfo('import/orange-dev-realm.json')
            entry.size, entry.uid, entry.gid, entry.mode = len(payload), 1000, 0, 0o600
            tar.addfile(entry, io.BytesIO(payload))
        subprocess.run(DOCKER+['cp', '-', existing['Id']+':/opt/keycloak/data/'], input=archive.getvalue(), check=True)
    if not existing['State']['Running']:
        subprocess.run(DOCKER+['start', existing['Id']], check=True, stdout=subprocess.DEVNULL)
    for _ in range(90):
        try:
            with urllib.request.urlopen(ISSUER+'/.well-known/openid-configuration', timeout=2) as response:
                data = json.load(response)
            if data['issuer'] != ISSUER:
                raise RuntimeError('issuer mismatch')
            print('Real local Keycloak 26.8.1 ready; loopback only, MFA flow configured; not production certification')
            return
        except (OSError, ValueError):
            time.sleep(2)
    raise SystemExit('Local identity did not become ready; inspect dedicated container logs privately')

if __name__ == '__main__':
    main()
