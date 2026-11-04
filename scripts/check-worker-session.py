#!/usr/bin/env python3
"""Actual localhost mTLS/gRPC and PostgreSQL. Ephemeral test CA, no public enrollment."""
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import tempfile
ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
environment=os.environ.copy();environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','')
if not environment.get('ORANGE_TEST_DATABASE_URL'):
    environment['ORANGE_TEST_DATABASE_URL']=(ROOT/'.local/database-url').read_text().strip()
openssl='/opt/homebrew/opt/openssl@3/bin/openssl'
if not Path(openssl).is_file():
    openssl=shutil.which('openssl')
if not openssl:raise SystemExit('OpenSSL required for isolated test certificates')
with tempfile.TemporaryDirectory(prefix='orange-worker-mtls-') as directory:
    private=Path(directory);private.chmod(0o700)
    def command(*arguments):
        subprocess.run([openssl,*arguments],cwd=private,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,check=True)
    for ca in ['ca','other-ca']:
        command('req','-x509','-newkey','ec','-pkeyopt','ec_paramgen_curve:P-256','-nodes','-batch','-days','1','-subj','/CN=Orange-'+ca,'-keyout',ca+'.key','-out',ca+'.pem','-addext','basicConstraints=critical,CA:TRUE','-addext','keyUsage=critical,keyCertSign,cRLSign')
    for name in ['server','client-a','client-b','client-c','client-d','client-e','client-f','client-g','client-h','client-i','client-j','client-k','client-l','unregistered','wrong-ca']:
        command('req','-new','-newkey','ec','-pkeyopt','ec_paramgen_curve:P-256','-nodes','-batch','-subj','/CN=orange-'+name,'-keyout',name+'.key','-out',name+'.csr')
        extensions='basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage='+('serverAuth' if name=='server' else 'clientAuth')+'\n'
        if name=='server':extensions+='subjectAltName=DNS:localhost,IP:127.0.0.1\n'
        (private/'leaf.ext').write_text(extensions)
        ca='other-ca' if name=='wrong-ca' else 'ca'
        command('x509','-req','-in',name+'.csr','-CA',ca+'.pem','-CAkey',ca+'.key','-set_serial',str(secrets.randbits(128)+1),'-days','1','-extfile','leaf.ext','-out',name+'.pem')
        command('x509','-in',name+'.pem','-outform','DER','-out',name+'.der')
    for file in private.iterdir():file.chmod(0o600)
    environment['ORANGE_WORKER_TLS_DIR']=str(private)
    subprocess.run([str(cargo),'test','-p','orange-control-plane','--features','integration-tests','--test','worker_sessions','--locked'],cwd=ROOT,env=environment,check=True)
    subprocess.run([str(cargo),'build','-p','orange-control-plane','--bin','orange-worker-gateway','--locked'],cwd=ROOT,env=environment,check=True)
print('PASS real localhost mTLS/gRPC/PostgreSQL identity, revocation and session epochs; test CA/dev certificate provisioning, no public enrollment or executed Job')
