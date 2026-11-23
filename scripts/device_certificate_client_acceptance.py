"""Run the real Worker through a loopback TLS fault proxy that forwards to the real development User API."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
import json
from pathlib import Path
import shutil
import socket
import ssl
import stat
import subprocess
import threading
from urllib.parse import urlsplit
import uuid


def check(worker,origin,root,device,ca,gw,issuer_hash,secret,rid,sql):
    def require(ok,msg):
        if not ok:raise ValueError(msg)
    def write(path,raw):
        path.write_bytes(raw);path.chmod(0o600)
    def json_file(path,data):
        write(path,json.dumps(data).encode())
    states={"mode":"drop","ids":[],"paths":[],"upstream":[]}
    mutex=threading.Lock()
    class Transport(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def do_POST(self):
            with mutex:states["paths"].append(self.path)
            if self.path=="/forbidden-redirect":
                self.send_error(400);return
            try:
                size=int(self.headers["Content-Length"])
                require(size<=4096 and self.path=="/api/v1/worker-enrollments/"+rid+"/redeem","Unexpected transport intent")
                raw=self.rfile.read(size);rb=json.loads(raw)
                require(self.headers["Authorization"]=="Orange-Enrollment "+secret and not self.headers.get("Cookie") and not self.headers.get("Origin"),"Unexpected authorization")
                require(self.headers["Idempotency-Key"]==rb["redemption_id"],"Unbound idempotency key")
                with mutex:
                    states["ids"].append(rb["redemption_id"]);mode=states["mode"]
                conn=HTTPConnection("127.0.0.1",urlsplit(origin).port,timeout=6)
                try:
                    conn.request("POST",self.path,raw,{"Content-Type":"application/json","Authorization":self.headers["Authorization"],"Idempotency-Key":rb["redemption_id"]})
                    answer=conn.getresponse();data=answer.read(32768)
                    require(answer.status==200,"Real backend refused the request")
                    body=json.loads(data)
                    with mutex:states["upstream"].append(body["node_id"])
                finally:conn.close()
                if mode=="drop":
                    self.connection.shutdown(socket.SHUT_RDWR);return
                if mode=="redirect":
                    self.send_response(307);self.send_header("Location","/forbidden-redirect");self.end_headers();return
                if mode=="wrong-request":body["request_id"]=str(uuid.uuid4())
                if mode=="wrong-key":body["spki_sha256"]="0"*64
                if mode=="wrong-expiry":body["expires_unix"]="9999999999"
                if mode=="wrong-leaf":
                    body["certificate_pem"]=(gw/"server.pem").read_text()
                    body["certificate_sha256"]=hashlib.sha256(ssl.PEM_cert_to_DER_cert(body["certificate_pem"])).hexdigest()
                if mode=="forged-signature":
                    damaged=bytearray(ssl.PEM_cert_to_DER_cert(body["certificate_pem"]));damaged[-1]^=1
                    body["certificate_pem"]=ssl.DER_cert_to_PEM_cert(bytes(damaged));body["certificate_sha256"]=hashlib.sha256(damaged).hexdigest()
                if mode=="extra-field":body["can_run_gpu"]=True
                response=b"x"*32769 if mode=="oversize" else json.dumps(body).encode()
                self.send_response(200);self.send_header("Content-Type","application/json");self.send_header("Content-Length",str(len(response)))
                self.end_headers();self.wfile.write(response)
            except (BrokenPipeError,ConnectionResetError):return
    tls=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);tls.load_cert_chain(gw/"server.pem",gw/"server.key")
    listener=ThreadingHTTPServer(("127.0.0.1",0),Transport)
    listener.socket=tls.wrap_socket(listener.socket,server_side=True)
    thread=threading.Thread(target=listener.serve_forever,daemon=True);thread.start()
    try:
        token=device/(rid+".exchange-token");require(token.read_text()==secret,"Missing original private token")
        issuer=device/"issuer-public.pem";write(issuer,(ca/"ca.pem").read_bytes())
        api=device/"api-public.pem";write(api,(root/"gateway-ca.pem").read_bytes())
        cfg={"version":1,"environment":"development","api_origin":f"https://localhost:{listener.server_port}","api_tls_ca_file":str(api),
             "device_directory":str(device),"request_id":rid,"exchange_token_file":str(token),"issuer_certificate":str(issuer),"issuer_sha256":issuer_hash}
        config=device/"redeem-config.json";confirmed=device/(rid+".receipt.json");pem=device/(rid+".client.pem");intent=device/(rid+".redemption.json")
        def invoke(positive=False):
            result=subprocess.run([worker,"redeem-enrollment",config],capture_output=True,timeout=15)
            require(secret.encode() not in result.stdout+result.stderr and b"PRIVATE KEY" not in result.stdout+result.stderr,"Sensitive Worker output")
            if positive:
                require(result.returncode==0,"Worker rejected genuine receipt: "+result.stderr.decode(errors="replace"));return json.loads(result.stdout)
            require(result.returncode!=0 and not result.stdout,"Worker accepted an unconfirmed enrollment")
        json_file(config,dict(cfg,issuer_sha256="0"*64));invoke()
        require(not states["ids"] and not intent.exists(),"Unreviewed issuer permitted a request")
        json_file(config,cfg);invoke()
        require(intent.exists() and not confirmed.exists() and not pem.exists() and len(set(states["upstream"]))==1,"First committed response was not lost cleanly")
        prior=json.loads(intent.read_bytes());ids=list(states["ids"])
        require(prior["redemption_id"]==ids[0] and secret.encode() not in intent.read_bytes(),"Secret or wrong identity in journal")
        write(token,b"A"*43);invoke();write(token,secret.encode())
        require(states["ids"]==ids,"Changed commitment reached transport")
        json_file(config,dict(cfg,api_origin=origin,api_tls_ca_file=None));invoke();json_file(config,cfg)
        for mode in ("redirect","wrong-request","wrong-key","wrong-expiry","wrong-leaf","forged-signature","extra-field","oversize"):
            states["mode"]=mode;invoke()
            require(not confirmed.exists() and not pem.exists(),"Invalid response published a credential: "+mode)
        require("/forbidden-redirect" not in states["paths"],"Redirect was followed")
        # Force the second local file to fail after a genuine immutable receipt is fsynced.
        states["mode"]="normal";marker=device/"owner-file";write(marker,b"keep this owner file")
        pem.symlink_to(marker);invoke()
        require(confirmed.is_file() and pem.is_symlink() and marker.read_bytes()==b"keep this owner file","Local publication crossed a file boundary")
        pem.unlink();token.unlink()
        lost=subprocess.run([worker,"prepare-enrollment-handoff",device,rid],capture_output=True)
        require(lost.returncode!=0 and not lost.stdout and not token.exists(),"Lost handoff secret was silently rotated")
        seen=len(states["ids"])
        with ThreadPoolExecutor(max_workers=2) as pool:
            a,b=list(pool.map(lambda _:invoke(True),range(2)))
        require(a==b and a["source"]=="local_receipt" and len(states["ids"])==seen,"Confirmed local repair performed another remote redemption")
        require(Path(a["certificate"])==pem and Path(a["private_key"])==device/(rid+".key.pem") and a["node_id"]==states["upstream"][0],"Installed identity paths or node changed")
        require(set(states["ids"])=={prior["redemption_id"]} and len(set(states["upstream"]))==1,"Ambiguous outcome created a second redemption")
        for f in (intent,pem,confirmed):require(stat.S_IMODE(f.stat().st_mode)==0o600 and f.stat().st_nlink==1,"Unprotected device state")
        require(sql("SELECT count(*) FROM worker_enrollment_certificates WHERE redeemed_at IS NOT NULL")=="1","Duplicate activation")
        print(json.dumps({"original_redemption_preserved":True,"committed_response_dropped":True,"bad_response_modes":8,
                          "confirmed_install_without_token_or_network":True,"private_key_remained_local":True}),flush=True)
        return json.loads(confirmed.read_bytes()),pem
    finally:
        listener.shutdown();thread.join(timeout=3);listener.server_close()
