"""Exercise the unpacked Linux Worker and gateway in one private loopback namespace with isolated role filesystems."""
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import re
import secrets
import struct
import subprocess
import uuid


def require(value,message):
    if not value:raise ValueError(message)


def check(call,create,ready,group,package,tool_image,pg,namespace,dburl,user_port,origin,sql):
    # No DB credentials are set on the device. No device private material is copied to the issuer or gateway.
    def container(name,command,program,db=False):
        args=["--network",namespace,"--user","65532:65532","--read-only","--cap-drop","ALL","--memory","160m","--cpus","0.8",
              "--tmpfs","/state:rw,nosuid,nodev,noexec,uid=65532,gid=65532,mode=0700",
              "--tmpfs","/tools:rw,nosuid,nodev,exec,uid=65532,gid=65532,mode=0700,size=64m"]
        if db:args+=["-e","DATABASE_URL","-e","ORANGE_ENV=development"]
        cid=create(group,name,[*args,tool_image,*command],dict(os.environ,DATABASE_URL=dburl) if db else None)
        call(["start",cid],capture_output=True)
        actual=(package/"bin"/program).read_bytes()
        manifest=json.loads((package/"manifest.json").read_bytes())
        expected=manifest["sha256"]["bin/"+program]
        require(hashlib.sha256(actual).hexdigest()==expected,"Extracted native binary differs")
        target="/tools/"+program
        call(["exec","-i",cid,"sh","-ec",'umask 077; cat > "$1"; chmod 0500 "$1"',"install",target],input=actual,capture_output=True)
        output=call(["exec",cid,"sha256sum",target],capture_output=True,text=True).stdout
        require(output.split()[0]==expected,"Isolated role received a different binary")
        return cid
    def execute(cid,*args,raw=None):
        return call(["exec",*(["-i"] if raw is not None else []),cid,*args],input=raw,capture_output=True,timeout=25).stdout
    def write(cid,path,data):
        # Path is a fixture basename; never put tokens or credentials into process arguments.
        require(re.fullmatch(r"[a-z0-9.-]+",path),"Invalid private fixture basename")
        execute(cid,"sh","-ec",'umask 077; cat > "/state/$1"',"write",path,raw=data)
    def put_json(cid,name,data):write(cid,name,json.dumps(data).encode())
    def cert(cid,*args):return execute(cid,"/usr/bin/openssl",*args)
    worker="/tools/orange-worker"
    gateway="/tools/orange-worker-gateway"
    device=container("enrolling-device",["sleep","infinity"],"orange-worker")
    signer=container("private-signer",["sleep","infinity"],"orange-worker-gateway",db=True)
    # The gateway generates its own private service key. A ready marker publishes only after the config is fully copied.
    server=container("device-gateway",["sh","-ec",'''
        umask 077; cd /state
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -batch -days 2 -subj '/CN=Test gateway trust' -keyout service-ca.key -out service-ca.pem -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign'
        openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -batch -subj '/CN=localhost' -keyout gateway.key -out gateway.csr
        printf '%s\n' 'basicConstraints=critical,CA:FALSE' 'keyUsage=critical,digitalSignature' 'extendedKeyUsage=serverAuth' 'subjectAltName=DNS:localhost' > gateway.ext
        openssl x509 -req -in gateway.csr -CA service-ca.pem -CAkey service-ca.key -set_serial 2 -days 1 -extfile gateway.ext -out gateway.pem
        until test -f config.ready; do sleep 0.1; done
        export ORANGE_WORKER_GATEWAY_BIND=127.0.0.1:43104 ORANGE_WORKER_GATEWAY_CONFIG=/state/gateway.json
        exec /tools/orange-worker-gateway > /state/ready.log 2>&1
    '''],"orange-worker-gateway",db=True)
    cert(signer,"req","-x509","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-batch","-days","2",
         "-subj","/CN=Private Linux device fixture","-keyout","/state/issuer.key","-out","/state/issuer.pem",
         "-addext","basicConstraints=critical,CA:TRUE","-addext","keyUsage=critical,keyCertSign,cRLSign")
    execute(signer,"chmod","600","/state/issuer.pem")
    issuer_pem=execute(signer,"cat","/state/issuer.pem");issuer_sha=hashlib.sha256(cert(signer,"x509","-in","/state/issuer.pem","-outform","DER")).hexdigest()
    put_json(signer,"issuer.json",dict(version=1,environment="development",region_id="dev-linux",ca_certificate_file="/state/issuer.pem",ca_key_file="/state/issuer.key",ca_certificate_sha256=issuer_sha))
    write(server,"clients.pem",issuer_pem);put_json(server,"gateway.json",dict(gateway_instance_id=str(uuid.uuid4()),server_certificate_file="/state/gateway.pem",
                server_key_file="/state/gateway.key",client_ca_file="/state/clients.pem"));write(server,"config.ready",b"ready\n")
    def gateway_ready():
        try:return b"Development standalone Worker mTLS control on 127.0.0.1:43104" in execute(server,"cat","/state/ready.log")
        except subprocess.CalledProcessError:return False
    ready(gateway_ready,server,"installed device Gateway",45)
    write(device,"gateway-ca.pem",execute(server,"cat","/state/service-ca.pem"));write(device,"issuer.pem",issuer_pem)
    rid,owner,session=[str(uuid.uuid4()) for _ in range(3)]
    login=secrets.token_hex(32);csrf=secrets.token_hex(32)
    sql(f"""INSERT INTO owners(owner_id) VALUES('{owner}');
    INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES('{owner}','linux-device-test','{owner}','{owner}');
    INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
    SELECT '{hashlib.sha256(login.encode()).hexdigest()}','{session}','{owner}','{csrf}',n,n+600,n+1200 FROM (SELECT extract(epoch FROM now())::bigint AS n)t;""")
    handoff=execute(device,worker,"prepare-enrollment-handoff","/state",rid)
    public=json.loads(handoff)
    require(set(public)=={"request","exchange_token_sha256"} and public["request"]["request_id"]==rid and len(public["exchange_token_sha256"])==64,"Invalid public device handoff")
    require(execute(device,worker,"prepare-enrollment-handoff","/state",rid)==handoff,"Unstable device request")
    path=f"/api/v1/owners/{owner}/worker-enrollments/{rid}"
    def manager(method,data=b""):
        wire=(f"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{user_port}\r\nOrigin: {origin}\r\nCookie: __Host-orange-user={login}\r\n"
              f"X-CSRF-Token: {csrf}\r\nIdempotency-Key: {rid}\r\nContent-Type: application/json\r\nContent-Length: {len(data)}\r\nConnection: close\r\n\r\n").encode()+data
        raw=execute(pg,"bash","-ec",'exec 3<>/dev/tcp/127.0.0.1/"$1"; cat >&3; cat <&3',"user",str(user_port),raw=wire)
        class Socket:
            def makefile(self,*_):return io.BytesIO(raw)
        with http.client.HTTPResponse(Socket()) as response:
            response.begin();require(response.status==200,"Owner enrollment operation refused")
            return json.loads(response.read(32768))
    require(manager("PUT",handoff)["state"]=="authorized","Missing owner grant")
    require(json.loads(execute(signer,gateway,"issue-one","/state/issuer.json"))["prepared_request"]==rid,"Wrong Linux signer receipt")
    require(manager("GET")["state"]=="certificate_prepared" and sql("SELECT count(*) FROM worker_credentials")=="0","Issuer incorrectly activated the device")
    put_json(device,"redeem.json",dict(version=1,environment="development",api_origin=f"http://127.0.0.1:{user_port}",device_directory="/state",request_id=rid,
                                     exchange_token_file=f"/state/{rid}.exchange-token",issuer_certificate="/state/issuer.pem",issuer_sha256=issuer_sha))
    installed=json.loads(execute(device,worker,"redeem-enrollment","/state/redeem.json"))
    node=str(uuid.UUID(installed["node_id"]));require(installed["source"]=="gateway_receipt" and installed["spki_sha256"]==public["request"]["spki_sha256"],"Wrong device identity")
    require(installed["certificate"]==f"/state/{rid}.client.pem" and installed["private_key"]==f"/state/{rid}.key.pem","Unexpected installed paths")
    # Only the two owned fixture files are removed to simulate recovery after receipt publication.
    execute(device,"rm","--",f"/state/{rid}.exchange-token",installed["certificate"])
    recovered=json.loads(execute(device,worker,"redeem-enrollment","/state/redeem.json"))
    require(recovered==dict(installed,source="local_receipt"),"Linux device did not repair the original confirmed PEM")
    cert(device,"verify","-purpose","sslclient","-CAfile","/state/issuer.pem",installed["certificate"])
    def proto(method,message,data):
        return subprocess.check_output(["protoc","--proto_path=.",f"--{method}=orange.worker.v1.{message}","worker.proto"],
                                       cwd=package/"contracts/proto",input=data)
    hello=proto("encode","WorkerFrame",f'hello {{ node_id: "{node}" protocol_version: 1 capabilities_digest: "{"a"*64}" }}'.encode())
    def control():
        execute(device,"/usr/bin/curl","--silent","--show-error","--max-time","12","--noproxy","*","--http2","--cacert","/state/gateway-ca.pem",
                "--cert",installed["certificate"],"--key",installed["private_key"],"--header","Content-Type: application/grpc","--header","te: trailers",
                "--dump-header","/state/headers","--output","/state/frame","--data-binary","@-","https://localhost:43104/orange.worker.v1.WorkerGateway/Control",
                raw=b"\x00"+struct.pack(">I",len(hello))+hello)
        return execute(device,"cat","/state/headers").decode().lower(),execute(device,"cat","/state/frame")
    headers,frame=control()
    require("http/2 200" in headers and len(frame)>5 and frame[0]==0,"Installed gateway did not return a gRPC frame")
    size=int.from_bytes(frame[1:5],"big");require(0<size<=len(frame)-5,"Incomplete real welcome")
    decoded=proto("decode","ServerFrame",frame[5:5+size]).decode()
    require("welcome {" in decoded and "connection_epoch: 1" in decoded and "session_id:" in decoded,"No real TLS control Welcome")
    require(manager("DELETE")["state"]=="revoked","Withdrawal failed")
    headers,frame=control()
    require("grpc-status: 16" in headers and not frame,"Old device reconnected after withdrawal")
    require(sql(f"SELECT count(*) FROM worker_session_audit WHERE node_id='{node}' AND kind='session.claimed'")=="1","Unexpected accepted sessions")
    require(sql(f"SELECT count(*) FROM worker_credentials WHERE node_id='{node}' AND active")=="0" and sql("SELECT count(*) FROM jobs")=="0","Unexpected execution authority")
    return {"real_handoff":True,"prepared_certificates":1,"real_redemption":True,"repaired_pem_without_token":True,"verified_grpc_welcome":True,
            "revoked_reconnect_denied":True,"independent_device_and_signer_filesystems":True,"model_jobs":0}
