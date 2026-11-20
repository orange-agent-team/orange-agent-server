#!/usr/bin/env python3
"""Measure an uploaded real recording at the user HTTP API, including live revocation."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError
from urllib.parse import unquote, urlsplit
from urllib.request import Request, urlopen
import uuid

from owned_postgres import database
from acceptance_origins import reserve_origins

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--database-url-file", type=Path, default=ROOT / ".local/database-url")
parser.add_argument("--identity-config", type=Path, default=ROOT / ".local/identity/user-config.json")
parser.add_argument("--model-cache", type=Path, default=ROOT / ".local/transcription-model")
parser.add_argument("--python", type=Path, default=ROOT / ".local/transcription-venv/bin/python")
parser.add_argument("--web-repo", type=Path)
args = parser.parse_args()
if args.web_repo:
    reserve_origins(43122)
    web = args.web_repo.resolve()
    node = Path.home()/".nvm/versions/node/v22.23.2/bin/node"
    subprocess.run([node, web/"scripts/check.mjs"],cwd=web,check=True)
    subprocess.run([sys.executable,web/"scripts/local-tls.py"],cwd=web,check=True)
if not __debug__:
    raise SystemExit("Verification requires enabled assertions")
rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo = Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
subprocess.run([cargo,"build","--offline","--locked","-p","orange-control-plane","--bin","orange-user-api"],
               cwd=ROOT,env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"]),check=True)
target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")).resolve()
origin = json.loads(args.identity_config.read_text())["user_origin"]
pin = json.loads((ROOT/"config/local-transcription.lock.json").read_text())["qualification_sample"]
raw = (args.model_cache/"jfk.wav").read_bytes()
assert hashlib.sha256(raw).hexdigest() == pin["sha256"]


def request(base, path, data, headers, status, method="POST"):
    if isinstance(data, dict):
        headers = dict(headers, **{"Content-Type":"application/json"})
        data = json.dumps(data).encode()
    req = Request(base + path, data=data, headers=headers, method=method)
    try:
        response = urlopen(req, timeout=35)
    except HTTPError as exc:
        response = exc
    with response:
        answer = json.load(response)
        assert response.status == status, (path, response.status, status, answer.get("code"))
        assert response.headers["Cache-Control"] == "no-store"
        return answer


def exercise(base, sql, folder):
    def session():
        user, sid = str(uuid.uuid4()), str(uuid.uuid4())
        token, csrf = secrets.token_hex(32), secrets.token_hex(32)
        digest = hashlib.sha256(token.encode()).hexdigest()
        sql(f"""INSERT INTO owners(owner_id) VALUES('{user}');
            INSERT INTO wallets(owner_id) VALUES('{user}');
            INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES('{user}','preflight-fixture','{user}','{user}');
            INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
            VALUES('{digest}','{sid}','{user}','{csrf}',0,
            floor(extract(epoch from clock_timestamp()))::bigint+600,floor(extract(epoch from clock_timestamp()))::bigint+600);""")
        return user, sid, {"Cookie":"__Host-orange-user="+token,"Origin":origin,"X-CSRF-Token":csrf}
    owner, sid, auth = session()
    _, _, outsider = session()
    prefix = "/api/v1/owners/"+owner+"/uploads"
    def publish(media, content):
        upload = str(uuid.uuid4())
        headers = {**auth, "Idempotency-Key":upload}
        response = request(base,prefix,{"upload_id":upload,"sha256":hashlib.sha256(content).hexdigest(),
            "size_bytes":str(len(content)),"media_type":media},headers,200)
        path = prefix+"/"+upload
        ready = request(base,path+"/content",content,headers,200,"PUT")
        assert ready["version_id"]==response["version_id"] and ready["state"]=="available"
        return path+"/transcription-measurements",{"version_id":ready["version_id"]},headers
    endpoint, body, headers = publish("audio/wav",raw)
    one = request(base,endpoint,body,headers,200)
    assert one["audio_sha256"]==pin["sha256"] and one["duration_ms"]=="11000" and one["source_samples"]=="176000"
    assert one["source_sample_rate"]==16000 and one["normalized_samples"]=="176000" and one["normalized_sha256"]=="a29462b8ebd467318000e683b9117ade46230d3255ed2024e7db894abd9b38c9"
    assert request(base,endpoint,body,headers,200)==one
    assert request(base,endpoint,body,{},401)["code"]=="UNAUTHENTICATED"
    bad = {**headers,"X-CSRF-Token":"wrong"}
    assert request(base,endpoint,body,bad,403)["code"]=="CSRF_INVALID"
    assert request(base,endpoint,body,{**headers,**outsider},403)["code"]=="FORBIDDEN"
    request(base,endpoint,dict(body,duration_ms="1"),headers,422)
    request(base,endpoint,{"version_id":"stale"},headers,409)
    for media, content in [("audio/flac",raw),("audio/wav",raw[:-2]),("text/plain",b"private text")]:
        route, version, hdr = publish(media,content)
        request(base,route,version,hdr,422)
    if args.web_repo:
        screenshot = ROOT/".local/audio-preflight-evidence/measurement.png"
        screenshot.parent.mkdir(mode=0o700,parents=True,exist_ok=True)
        browser = {"api":base,"origin":origin,"owner":owner,"token":auth["Cookie"].split("=",1)[1],
                   "csrf":auth["X-CSRF-Token"],"sample":str((args.model_cache/"jfk.wav").resolve()),
                   "sha":pin["sha256"],"screenshot":str(screenshot)}
        subprocess.run([node,web/"apps/web/scripts/real-audio-preflight.mjs"],
                       input=json.dumps(browser),text=True,cwd=web,check=True,timeout=180)
    (folder/"arm").touch()
    with ThreadPoolExecutor(max_workers=1) as executor:
        future = executor.submit(request,base,endpoint,body,headers,403)
        try:
            deadline = time.monotonic()+12
            while not (folder/"entered").exists():
                if future.done(): future.result()
                if time.monotonic()>=deadline: raise TimeoutError("Decoder admission marker missing")
                time.sleep(.02)
            assert request(base,endpoint,body,headers,409)["code"]=="TRANSFER_BUSY"
            sql(f"UPDATE user_sessions SET revoked=true WHERE session_id='{sid}'")
        finally:
            (folder/"release").touch()
        assert future.result(timeout=20)["code"]=="FORBIDDEN"
    assert sql("SELECT (SELECT count(*) FROM quotes)+(SELECT count(*) FROM jobs)")=="0"
    print(json.dumps({"real_http":True,"recorded_audio_sha256":pin["sha256"],"duration_ms":11000,"samples":176000,
                      "repeated_measurement_identical":True,"fixture_session_revoked_before_delivery":True,
                      "overlapping_decode_denied":True,"quotes_or_jobs_created":0}))


with database(args.database_url_file,"orange_audio_") as (_,url), tempfile.TemporaryDirectory(prefix="orange-audio-scope-") as tmp:
    folder=Path(tmp)
    slot, objects=folder/"slot",folder/"objects"
    slot.mkdir(mode=0o700);objects.mkdir(mode=0o700)
    wrapper=folder/"test-python"
    # Only the test injects a pause before exec. The actual interpreter then decodes the real object.
    wrapper.write_text("#!/usr/bin/python3\nimport os,sys,time\nfrom pathlib import Path\n"
        +"p=Path("+repr(str(folder))+")\nif (p/'arm').exists():\n (p/'entered').touch()\n"
        +" for _ in range(600):\n  if (p/'release').exists(): break\n  time.sleep(.05)\n else: sys.exit(2)\n"
        +"v="+repr(str(args.python.absolute()))+"\nos.execv(v,[v,*sys.argv[1:]])\n")
    wrapper.chmod(0o700)
    cfg=folder/"meter.json";cfg.write_text(json.dumps({"python":str(wrapper),"model_cache":str(args.model_cache.resolve()),"private_slot":str(slot)}));cfg.chmod(0o600)
    env=dict(os.environ,ORANGE_ENV="development",ORANGE_USER_BIND="127.0.0.1:0",DATABASE_URL=url,
             ORANGE_USER_CONFIG_FILE=str(args.identity_config.resolve()),ORANGE_ARTIFACT_ROOT=str(objects),ORANGE_AUDIO_METER_CONFIG=str(cfg))
    db=urlsplit(url)
    pg=dict(os.environ,PGHOST=db.hostname,PGPORT=str(db.port),PGUSER=unquote(db.username or ""),
            PGPASSWORD=unquote(db.password or ""),PGDATABASE=unquote(db.path[1:]),PGCONNECT_TIMEOUT="5")
    psql=shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
    def sql(statement):
        return subprocess.check_output([psql,"-X","-w","-qAt","-v","ON_ERROR_STOP=1"],input=statement,text=True,env=pg).strip()
    with subprocess.Popen([target/"debug/orange-user-api"],cwd=ROOT,env=env,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,text=True) as proc:
        try:
            with selectors.DefaultSelector() as sel:
                sel.register(proc.stderr,selectors.EVENT_READ)
                deadline=time.monotonic()+45
                while time.monotonic()<deadline:
                    if proc.poll() is not None: raise RuntimeError("Owned API exited before startup")
                    if sel.select(.5):
                        m=re.search(r"listening on (127\.0\.0\.1:\d+)",proc.stderr.readline())
                        if m: break
                else: raise TimeoutError("Owned API startup deadline")
            exercise("http://"+m.group(1),sql,folder)
        finally:
            (folder/"release").touch()
            if proc.poll() is None:
                proc.send_signal(signal.SIGINT)
                try: proc.wait(timeout=15)
                except subprocess.TimeoutExpired: proc.kill();proc.wait()
