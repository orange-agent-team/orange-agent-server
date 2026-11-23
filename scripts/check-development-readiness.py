#!/usr/bin/env python3
"""Read the real local environment and exercise rejection paths on test-owned configurations."""
import argparse
import importlib.util
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import subprocess
import sys
import tempfile
import threading

script=Path(__file__).with_name("development-readiness.py")
spec=importlib.util.spec_from_file_location("orange_readiness",script)
service=importlib.util.module_from_spec(spec)
spec.loader.exec_module(service)


def rejected(function,*args):
    try:
        function(*args)
    except (service.ConfigurationError,service.Unavailable,OSError):
        return
    raise ValueError("Expected readiness rejection")


def write(path,value):
    path.write_text(json.dumps(value))
    path.chmod(0o600)


def test_private_configs(base):
    data=base/"postgres/data"
    data.mkdir(parents=True)
    (base/"identity").mkdir()
    write(data.parent/"identity.json",dict(data_directory=str(data),purpose="orange-development-only",port=55432,production=False))
    url=base/"database-url"
    url.write_text("postgresql://orange_local:private-marker@127.0.0.1:55432/orange_dev")
    url.chmod(0o600)
    for role,port in (("user",43122),("admin",43112)):
        write(base/"identity"/(role+"-config.json"),dict(environment="dev",issuer="http://127.0.0.1:44180/realms/orange-dev",
              client_id=role,client_secret="private-marker",**{role+"_origin":"https://127.0.0.1:"+str(port)}))
    service.config(base)
    url.chmod(0o644)
    rejected(service.config,base)
    url.chmod(0o600)
    url.write_text("postgresql://orange_local:private-marker@external.invalid:55432/orange_dev")
    rejected(service.config,base)
    result=subprocess.run([sys.executable,script,"--environment",base],capture_output=True,text=True)
    if result.returncode!=2 or "private-marker" in result.stderr+result.stdout:
        raise ValueError("Unexpected rejection status or credential disclosure")


def test_discovery():
    paths=[]
    class Fixture(BaseHTTPRequestHandler):
        def log_message(self,*args):
            pass
        def do_GET(self):
            paths.append(self.path)
            self.send_response(200)
            self.end_headers()
            payload={"issuer":origin,"token_endpoint":origin+"/token","authorization_endpoint":origin+"/auth",
                     "jwks_uri":"http://external.invalid/keys"}
            self.wfile.write(json.dumps(payload).encode())
    server=HTTPServer(("127.0.0.1",0),Fixture)
    origin="http://127.0.0.1:"+str(server.server_port)+"/realms/orange-dev"
    thread=threading.Thread(target=server.serve_forever)
    thread.start()
    try:
        rejected(service.oidc,service.urlsplit(origin))
        if paths!=["/realms/orange-dev/.well-known/openid-configuration"]:
            raise ValueError("Unexpected outbound discovery request")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    rejected(service.oidc,service.urlsplit(origin))


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--environment",type=Path,default=script.parents[1]/".local")
    args=parser.parse_args()
    with tempfile.TemporaryDirectory() as tmp:
        test_private_configs(Path(tmp).resolve())
        external=subprocess.run([sys.executable,script,"--start","--environment",tmp],capture_output=True,text=True)
        if external.returncode!=2:
            raise ValueError("Provisioning outside the owning checkout must fail")
    test_discovery()
    subprocess.run([sys.executable,script,"--environment",args.environment,"--wait-seconds","4"],check=True)
    print("PASS private state, wrong cluster authority, credential redaction, issuer endpoint rejection, closed socket and actual local readiness")


if __name__=="__main__":
    main()
