"""Issue temporary, separately trusted platform Host and foreign test certificates."""
import shutil
import subprocess
from pathlib import Path

def prepare(tls):
    openssl=Path('/opt/homebrew/opt/openssl@3/bin/openssl')
    if not openssl.is_file():openssl=shutil.which('openssl')
    if not openssl or not subprocess.check_output([openssl,'version'],text=True).startswith('OpenSSL 3.'):
        raise SystemExit('OpenSSL 3 is required for the private test PKI')
    def issue(command):
        subprocess.run([openssl,*command],cwd=tls,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,check=True)
    for ca in ['host-ca','external-ca']:
        issue(['req','-x509','-newkey','ec','-pkeyopt','ec_paramgen_curve:P-256','-noenc','-subj','/CN='+ca,
               '-days','1','-addext','basicConstraints=critical,CA:TRUE','-addext','keyUsage=critical,keyCertSign,cRLSign',
               '-keyout',ca+'.key','-out',ca+'.pem'])
    for name,ca,usage in [('gateway','host-ca','serverAuth'),('host','host-ca','clientAuth'),('unadmitted','host-ca','clientAuth'),('worker','external-ca','clientAuth')]:
        issue(['req','-new','-newkey','ec','-pkeyopt','ec_paramgen_curve:P-256','-noenc','-subj','/CN='+name,'-keyout',name+'.key','-out',name+'.csr'])
        ext='basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage='+usage+'\n'
        if name=='gateway':ext+='subjectAltName=DNS:localhost,IP:127.0.0.1\n'
        (tls/'extensions').write_text(ext)
        issue(['x509','-req','-in',name+'.csr','-CA',ca+'.pem','-CAkey',ca+'.key','-CAcreateserial','-days','1','-extfile','extensions','-out',name+'.pem'])
        issue(['x509','-in',name+'.pem','-outform','DER','-out',name+'.der'])
