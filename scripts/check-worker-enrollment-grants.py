#!/usr/bin/env python3
"""Verify real on-device PKCS#10 approval at the User API with private disposable PostgreSQL."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import uuid

from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--environment",type=Path,default=ROOT/".local")
    private=parser.parse_args().environment.resolve()
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","cargo"],cwd=ROOT,text=True).strip())
    env={k:v for k,v in os.environ.items() if not k.startswith("ORANGE_")}
    env.update(PATH=str(cargo.parent)+os.pathsep+env["PATH"],CARGO_NET_OFFLINE="true",ORANGE_TEST_USER_CONFIG_FILE=str(private/"identity/user-config.json"))
    subprocess.run([cargo,"build","--locked","--offline","-p","orange-worker","--bin","orange-worker"],cwd=ROOT,env=env,check=True)
    metadata=json.loads(subprocess.check_output([cargo,"metadata","--no-deps","--format-version","1","--locked"],cwd=ROOT,env=env))
    worker=Path(metadata["target_directory"])/"debug/orange-worker"
    openssl="/opt/homebrew/opt/openssl@3/bin/openssl"
    if not Path(openssl).is_file():openssl=shutil.which("openssl")
    with tempfile.TemporaryDirectory(prefix="orange-enrollment-grants-") as temp:
        owner=Path(temp)/"device";owner.mkdir(mode=0o700)
        vectors={name:json.loads(subprocess.check_output([worker,"prepare-enrollment",owner,str(uuid.uuid4())])) for name in ("personal","team","expired","quota")}
        original=vectors["personal"];rid=original["request_id"]
        key=owner/(rid+".key.pem")
        def normalized(pem):
            der=base64.b64decode("".join(s for s in pem.splitlines() if not s.startswith("-----")),validate=True)
            return dict(original,csr_sha256=hashlib.sha256(der).hexdigest(),csr_pem=pem)
        def cryptographic_negative(subject,usages,eku,extras=()):
            cmd=[openssl,"req","-new","-batch","-key",key,"-subj",subject,"-addext","keyUsage=critical,"+usages,"-addext","extendedKeyUsage="+eku]
            for ext in extras:cmd.extend(["-addext",ext])
            return normalized(subprocess.check_output(cmd,stderr=subprocess.PIPE,text=True))
        cn="/CN=orange-worker-"+rid
        bad=bytearray(base64.b64decode("".join(s for s in original["csr_pem"].splitlines() if not s.startswith("-"))))
        bad[-1]^=1
        vectors["malformed"]=[normalized("-----BEGIN CERTIFICATE REQUEST-----\n"+base64.b64encode(bad).decode()+"\n-----END CERTIFICATE REQUEST-----\n"),
            dict(original,spki_sha256="a"*64),dict(original,purpose="platform_host"),dict(original,csr_pem=original["csr_pem"]*2),
            cryptographic_negative("/CN=other","digitalSignature","clientAuth"),cryptographic_negative(cn,"digitalSignature","clientAuth,serverAuth"),
            cryptographic_negative(cn,"digitalSignature,keyCertSign","clientAuth",["basicConstraints=critical,CA:TRUE"]),
            cryptographic_negative(cn,"digitalSignature","clientAuth",["subjectAltName=DNS:localhost"])]
        public=Path(temp)/"requests.json";public.write_text(json.dumps(vectors))
        if b"PRIVATE KEY" in public.read_bytes():raise ValueError("Private key in public fixtures")
        with database(private/"database-url","orange_enroll_grants_") as (_,url):
            env.update(ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_ENROLLMENT_VECTORS=str(public))
            result=subprocess.run([cargo,"test","--offline","--locked","-p","orange-control-plane","--features","worker-enrollment-tests",
                    "--test","user_identity","worker_enrollments::","--","--test-threads=1"],cwd=ROOT,env=env,text=True,stdout=subprocess.PIPE)
            print(result.stdout,end="",flush=True);result.check_returncode()
            for case in ("owner_approvals_verify_real_pkcs10_once_and_recheck_team_scope","expired_worker_approval_keeps_original_deadline_and_aged_request_is_pruned"):
                if "test worker_enrollments::"+case+" ... ok" not in result.stdout:raise RuntimeError("Missing "+case)
    print("PASS actual E08 CSR, owner and team authority, replay, revocation, age and denied foreign use; no certificate issuance or supply grant")


if __name__=="__main__":
    main()
