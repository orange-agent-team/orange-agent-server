"""Run an approved and a rejected repository listing with the CPU model, browser and local provider."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from threading import Thread

ROOT=Path(__file__).resolve().parents[1]

def main():
    p=argparse.ArgumentParser()
    p.add_argument("--environment",type=Path,default=ROOT/".local")
    p.add_argument("--web-repo",type=Path,required=True)
    p.add_argument("--node",type=Path,required=True)
    p.add_argument("--provider",choices=("github","gitlab"),required=True)
    a=p.parse_args();private=a.environment.resolve()
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","cargo"],cwd=ROOT,text=True).strip())
    env=dict(os.environ,PATH=os.pathsep.join([str(cargo.parent),str(a.node.resolve().parent),os.environ["PATH"]]),CARGO_NET_OFFLINE="true")
    build=subprocess.check_output([cargo,"build","--offline","--locked","-p","orange-control-plane","--example","hosted-repository-api-fixture",
            "--features","repository-fixture-tests","--message-format=json-render-diagnostics"],cwd=ROOT,env=env,text=True)
    programs=[r["executable"] for r in map(json.loads,build.splitlines()) if r.get("executable") and r["target"]["name"]=="hosted-repository-api-fixture"]
    if len(programs)!=1:raise RuntimeError("Missing isolated fixture API")
    hits=[];unexpected=[]
    expected="/repos/acme/notes" if a.provider=="github" else "/api/v4/projects/17"
    class Provider(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path!=expected or self.headers.get("Authorization")!="Bearer local-read-token":
                unexpected.append(self.path);self.send_error(403);return
            hits.append(self.path)
            field="full_name" if a.provider=="github" else "path_with_namespace"
            body=json.dumps({"id":17,field:"acme/notes","default_branch":"main"}).encode()
            self.send_response(200);self.send_header("Content-Type","application/json")
            self.send_header("Content-Length",str(len(body)));self.end_headers();self.wfile.write(body)
        def log_message(self,*_):pass

    with tempfile.TemporaryDirectory(prefix="orange-model-repo-") as tmp,ThreadingHTTPServer(("127.0.0.1",0),Provider) as server:
        fixture=Path(tmp)/"provider.json";fixture.write_text(json.dumps({"provider":a.provider,"address":f"127.0.0.1:{server.server_port}"}));fixture.chmod(0o600)
        thread=Thread(target=server.serve_forever,daemon=True);thread.start()
        try:
            env.update(ORANGE_MODEL_REPOSITORY_FIXTURE=str(fixture),ORANGE_TEST_REPOSITORY_API=programs[0],
                       ORANGE_CLOUD_TEST_GVISOR_ARCHIVE=str(private/"gvisor-download/gvisor-aarch64.tar.bz2"))
            result=subprocess.run([sys.executable,ROOT/"scripts/check-cloud-model-turn.py","--web-repo",a.web_repo.resolve(),
                "--node",a.node.resolve(),"--database-url-file",private/"database-url","--identity-config",private/"identity/user-config.json",
                "--model-runtime-root",private/"model-runtime"],cwd=ROOT,env=env,check=True)
            if result.returncode or hits!=[expected] or unexpected:raise RuntimeError(f"Wrong provider effects: expected one scoped read, received {len(hits)} accepted, {len(unexpected)} refused")
        finally:server.shutdown();thread.join()
    print("PASS hosted "+a.provider+" repository listing: actual CPU model, separate browser approval, one local provider read, no rejected or replayed read, purged source result")
if __name__=="__main__":main()
