#!/usr/bin/env python3
"""Verify the ordinary Worker command, real PKCS#10 signature, crash recovery and concurrent first use."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import nullcontext
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import ssl
import stat
import subprocess
import tempfile
import uuid

from verified_platform_archive import binaries

ROOT=Path(__file__).resolve().parents[1]


def require(condition, message):
    if not condition:raise ValueError(message)


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--installed", action="store_true")
    args=parser.parse_args()
    openssl="/opt/homebrew/opt/openssl@3/bin/openssl"
    if not Path(openssl).is_file():openssl=shutil.which("openssl")
    require(openssl,"OpenSSL required")
    if args.installed:
        context=binaries(None)
    else:
        rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
        cargo=Path(subprocess.check_output([rustup,"which","cargo"],cwd=ROOT,text=True).strip())
        env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"],CARGO_NET_OFFLINE="true")
        subprocess.run([cargo,"build","--offline","--locked","-p","orange-worker","--bin","orange-worker"],cwd=ROOT,env=env,check=True)
        metadata=json.loads(subprocess.check_output([cargo,"metadata","--no-deps","--format-version","1","--locked"],cwd=ROOT,env=env))
        context=nullcontext(Path(metadata["target_directory"])/"debug")
    with context as bindir,tempfile.TemporaryDirectory(prefix="orange-device-request-") as tmp:
        base=Path(tmp);root=base/"owner";root.mkdir(mode=0o700)
        worker=bindir/"orange-worker"
        def run(rid,directory=root,success=True):
            result=subprocess.run([worker,"prepare-enrollment",directory,str(rid)],capture_output=True,timeout=10)
            require(b"PRIVATE KEY" not in result.stdout+result.stderr,"Private material in output")
            if success:
                require(result.returncode==0,"Worker request failed: "+result.stderr.decode(errors="replace"))
                return json.loads(result.stdout)
            require(result.returncode!=0 and not result.stdout,"Unsafe key input accepted")
        def crypto(*options,data=None):
            return subprocess.check_output([openssl,*options],input=data,cwd=base,stderr=subprocess.PIPE)
        rid=uuid.uuid4()
        with ThreadPoolExecutor(max_workers=4) as pool:
            results=list(pool.map(lambda _:run(rid),range(8)))
        receipt=results[0]
        require(all(r==receipt for r in results),"Inconsistent concurrent CSR or public identity")
        require(set(receipt)=={"version","purpose","request_id","spki_sha256","csr_sha256","csr_pem"} and
                receipt["version"]==1 and receipt["purpose"]=="public_worker" and receipt["request_id"]==str(rid),"Wrong request shape")
        key=root/(str(rid)+".key.pem");csr=base/"worker.csr";csr.write_text(receipt["csr_pem"])
        require(list(root.iterdir())==[key] and stat.S_IMODE(key.stat().st_mode)==0o600 and key.stat().st_nlink==1,"Unexpected private key storage")
        crypto("req","-in",csr,"-verify","-noout")
        subject=crypto("req","-in",csr,"-subject","-noout","-nameopt","RFC2253").decode().strip()
        require(subject=="subject=CN=orange-worker-"+str(rid),"Wrong subject")
        text=crypto("req","-in",csr,"-text","-noout").decode()
        require("ED25519" in text.upper() and "TLS Web Client Authentication" in text and "Digital Signature" in text
                and "CA:TRUE" not in text and "TLS Web Server Authentication" not in text,"Wrong requested leaf usages")
        spki=crypto("pkey","-pubin","-outform","DER",data=crypto("req","-in",csr,"-pubkey","-noout"))
        require(spki==crypto("pkey","-in",key,"-pubout","-outform","DER") and hashlib.sha256(spki).hexdigest()==receipt["spki_sha256"],"Wrong public key")
        csr_der=crypto("req","-in",csr,"-outform","DER")
        require(hashlib.sha256(csr_der).hexdigest()==receipt["csr_sha256"],"Wrong PKCS#10 digest")
        # Independently verify proof of possession, including a damaged signature control.
        broken=bytearray(csr_der);broken[-1]^=1
        rejected=subprocess.run([openssl,"req","-verify","-noout","-inform","DER"],input=broken,capture_output=True)
        require(b"verify failure" in rejected.stderr.lower() or rejected.returncode!=0,"Invalid CSR signature verified")
        # The isolated fixture issuer handles only the public request. Its client leaf must
        # authenticate the original device key, and must be rejected for server authentication.
        crypto("req","-x509","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-batch","-days","1",
               "-subj","/CN=Enrollment fixture CA","-keyout","issuer.key","-out","issuer.pem",
               "-addext","basicConstraints=critical,CA:TRUE","-addext","keyUsage=critical,keyCertSign,cRLSign")
        (base/"client.ext").write_text("basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n")
        crypto("x509","-req","-in",csr,"-CA","issuer.pem","-CAkey","issuer.key","-set_serial","1","-days","1","-extfile","client.ext","-out","device.pem")
        crypto("verify","-purpose","sslclient","-CAfile","issuer.pem","device.pem")
        server_use=subprocess.run([openssl,"verify","-purpose","sslserver","-CAfile","issuer.pem","device.pem"],cwd=base,capture_output=True)
        require(server_use.returncode!=0,"Device leaf accepted as a server credential")
        crypto("req","-new","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-batch","-subj","/CN=localhost",
               "-keyout","gateway.key","-out","gateway.csr")
        (base/"server.ext").write_text("basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n")
        crypto("x509","-req","-in","gateway.csr","-CA","issuer.pem","-CAkey","issuer.key","-set_serial","2","-days","1","-extfile","server.ext","-out","gateway.pem")
        client=ssl.create_default_context(cafile=str(base/"issuer.pem"))
        client.load_cert_chain(base/"device.pem",key)
        server=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        server.verify_mode=ssl.CERT_REQUIRED
        server.load_cert_chain(base/"gateway.pem",base/"gateway.key");server.load_verify_locations(cafile=base/"issuer.pem")
        expected_leaf=ssl.PEM_cert_to_DER_cert((base/"device.pem").read_text())
        # A socket pair exercises a full TLS handshake without opening a network listener.
        left,right=socket.socketpair()
        with left,right,ThreadPoolExecutor(max_workers=1) as tls_pool:
            left.settimeout(5);right.settimeout(5)
            def accept():
                with server.wrap_socket(left,server_side=True) as stream:
                    require(stream.getpeercert(binary_form=True)==expected_leaf,"Wrong authenticated peer")
                    require(stream.recv(1)==b"q","Missing authenticated request")
                    stream.sendall(b"r")
            confirmed=tls_pool.submit(accept)
            with client.wrap_socket(right,server_hostname="localhost") as channel:
                channel.sendall(b"q");require(channel.recv(1)==b"r","No authenticated TLS reply")
            confirmed.result(timeout=6)
        other=uuid.uuid4();pending=root/("."+str(other)+".key.pending")
        pending.write_bytes(b"interrupted prepublication write");pending.chmod(0o600)
        resumed=run(other);require(run(other)==resumed and resumed["spki_sha256"]!=receipt["spki_sha256"] and not pending.exists(),"Torn unpublished key recovery failed")
        key_mode=key.stat().st_mode;key.chmod(0o644)
        run(rid,success=False);key.chmod(stat.S_IMODE(key_mode))
        alias=base/"alias";alias.symlink_to(root,target_is_directory=True);run(rid,alias,False)
        third=uuid.uuid4();linked=root/(str(third)+".key.pem");linked.symlink_to(key);run(third,success=False);linked.unlink()
        linked.hardlink_to(key);run(third,success=False);run(rid,success=False);linked.unlink()
        run("invalid",success=False)
        run(str(rid).upper(),success=False)
        root.chmod(0o755);run(rid,success=False);root.chmod(0o700)
        guard=uuid.uuid4();bad=root/(str(guard)+".key.pem")
        bad.write_bytes(b"damaged published key");bad.chmod(0o600);run(guard,success=False)
        require(bad.read_bytes()==b"damaged published key" and run(rid)==receipt,"Published identity was altered")
        print(json.dumps({"installed_worker":args.installed,"concurrent_processes":8,"independent_pkcs10_verification":True,
                          "same_key_after_restart":True,"partial_staging_recovered":True,"fixture_client_leaf_mtls":True,"platform_issuer_verified":False}),flush=True)
    print("PASS durable on-device key and real PKCS10 possession, bound to an independently checked public request")


if __name__=="__main__":
    main()
