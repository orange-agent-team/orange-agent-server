"""Verify rotation possession and request binding with an independent OpenSSL verifier."""
import base64
from concurrent.futures import ThreadPoolExecutor
import copy
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import uuid


def qualify(worker,device,owner,previous_id,receipt,sql):
    def require(ok,message):
        if not ok: raise ValueError(message)
    counts="SELECT (SELECT count(*) FROM worker_credentials),(SELECT count(*) FROM worker_enrollment_grants),(SELECT count(*) FROM nodes)"
    before=sql(counts)
    new=str(uuid.uuid4())
    def invoke(subject=owner,request=new):
        return subprocess.run([worker,"prepare-certificate-rotation",device/"redeem-config.json",subject,request],
                              capture_output=True,timeout=20)
    with ThreadPoolExecutor(max_workers=2) as pool:
        results=list(pool.map(lambda _:invoke(),range(2)))
    require(all(r.returncode==0 for r in results) and results[0].stdout==results[1].stdout,"Concurrent rotation candidates diverged")
    handoff=json.loads(results[0].stdout)
    intent=handoff["intent"]
    require(intent["previous_request_id"]==previous_id and intent["node_id"]==receipt["node_id"] and intent["owner_id"]==owner and
            intent["previous_certificate_sha256"]==receipt["certificate_sha256"] and intent["previous_issuer_sha256"]==receipt["issuer_sha256"],"Wrong installed binding")
    candidate=intent["replacement"]
    require(candidate["request"]["request_id"]==new and candidate["request"]["spki_sha256"]!=receipt["spki_sha256"],"Replacement reused old identity")
    token=(device/(new+".exchange-token")).read_bytes()
    require(token not in results[0].stdout and b"PRIVATE KEY" not in results[0].stdout and hashlib.sha256(token).hexdigest()==candidate["exchange_token_sha256"],
            "Leaked or mismatched new exchange secret")
    openssl=Path("/opt/homebrew/opt/openssl@3/bin/openssl")
    if not openssl.is_file(): openssl=shutil.which("openssl")
    require(openssl,"OpenSSL required")
    with tempfile.TemporaryDirectory() as tmp:
        files=Path(tmp)
        key=files/"old.pub"
        key.write_bytes(subprocess.check_output([openssl,"x509","-in",device/(previous_id+".client.pem"),"-pubkey","-noout"]))
        csr=files/"new.csr";csr.write_text(candidate["request"]["csr_pem"])
        subprocess.run([openssl,"req","-in",csr,"-verify","-noout"],check=True,capture_output=True)
        replacement_key=files/"new.pub"
        replacement_key.write_bytes(subprocess.check_output([openssl,"req","-in",csr,"-pubkey","-noout"]))
        signature=files/"signature"
        signature.write_bytes(base64.urlsafe_b64decode(handoff["previous_key_signature"]+"=="))
        def verifies(value,keyfile):
            message=files/"message"
            canonical=json.dumps(value,sort_keys=True,ensure_ascii=False,separators=(",",":")).encode()
            message.write_text(hashlib.sha256(b"orange-request-v1\0worker-certificate-rotation.v1\0"+canonical).hexdigest())
            return subprocess.run([openssl,"pkeyutl","-verify","-pubin","-inkey",keyfile,"-rawin","-in",message,"-sigfile",signature],
                                  capture_output=True,timeout=10).returncode==0
        require(verifies(intent,key) and not verifies(intent,replacement_key),"Expected only the original installed key to authorize rotation")
        for path in [("owner_id",),("node_id",),("previous_request_id",),("previous_certificate_sha256",),("previous_issuer_sha256",),
                     ("replacement","exchange_token_sha256"),("replacement","request","spki_sha256"),("replacement","request","csr_pem")]:
            altered=copy.deepcopy(intent);leaf=altered
            for item in path[:-1]:leaf=leaf[item]
            leaf[path[-1]]+="a"
            require(not verifies(altered,key),"Accepted a changed signed field")
    for result in (invoke(str(uuid.uuid4())),invoke(owner,previous_id)):
        require(result.returncode!=0 and not result.stdout,"Rebound a published rotation request")
    require(invoke().stdout==results[0].stdout and sql(counts)==before,"Preparation mutated service authority")
    token_file=device/(new+".exchange-token");saved_token=device/(new+".held-by-test");token_file.rename(saved_token)
    denied=invoke()
    require(denied.returncode!=0 and not denied.stdout and not token_file.exists(),"Lost rotation token was silently replaced")
    # Restore only the test-owned secret for the subsequent server redemption exercise.
    saved_token.rename(token_file)
    print(json.dumps({"previous_key_signature_verified":True,"replacement_csr_verified":True,"altered_signed_fields_rejected":8,
                      "concurrent_requests_identical":True,"raw_token_exported":False,"lost_secret_reissued":False,
                      "server_mutations_during_preparation":0}),flush=True)
    return handoff
