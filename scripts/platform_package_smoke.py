"""Exercise installed loopback platform binaries against one disposable PostgreSQL database."""
from contextlib import ExitStack,contextmanager
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
import time
from urllib.error import HTTPError
from urllib.parse import parse_qs,unquote,urlsplit
from urllib.request import HTTPRedirectHandler,Request,build_opener
import uuid

from owned_postgres import database


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self,*args):return None


def request(origin, path, cookie=None):
    req=Request(origin+path,headers={"Cookie":cookie} if cookie else {})
    try:response=build_opener(NoRedirect()).open(req,timeout=8)
    except HTTPError as exc:response=exc
    with response:
        return response.status,response.headers,response.read(65537)


@contextmanager
def listening(binary, env, label):
    process=subprocess.Popen([binary],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,text=True)
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(process.stderr,selectors.EVENT_READ)
            deadline=time.monotonic()+35
            while time.monotonic()<deadline:
                if process.poll() is not None:raise RuntimeError(label+" exited before listening")
                if selector.select(.25):
                    line=process.stderr.readline()
                    match=re.search(label+r".*?listening on (127\.0\.0\.1:\d+)",line,re.I)
                    if match:break
            else:raise TimeoutError(label+" readiness deadline")
        yield "http://"+match[1]
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill();process.wait();raise
        process.stderr.close()
    if process.returncode!=0:raise RuntimeError(label+" graceful exit failed")


def check_running(user,admin,user_config,admin_config,request,sql,migration_count):
    """Exercise the same independent identities and accounting database on both binary targets."""
    assert user!=admin
    diagnostic_ids=set()
    for origin,route in [(user,"/api/v1/session"),(admin,"/admin/v1/session")]:
        status,headers,body=request(origin,route)
        normalized={k.lower():v for k,v in headers.items()}
        assert status==401 and normalized["x-orange-error-class"]=="permission"
        assert normalized["x-orange-retry-class"]==json.loads(body)["retry_class"]
        diagnostic_ids.add(uuid.UUID(normalized["x-orange-diagnostic-id"]))
    assert len(diagnostic_ids)==2
    assert request(user,"/admin/v1/session")[0]==404 and request(admin,"/api/v1/session")[0]==404
    clients=[]
    for origin,path,cfg in [(user,"/api/v1/auth/start",user_config),(admin,"/admin/v1/auth/start",admin_config)]:
        config=json.loads(cfg.read_bytes());status,headers,_=request(origin,path)
        assert status in (302,303,307)
        where=urlsplit(headers["Location"]);issuer=urlsplit(config["issuer"])
        assert (where.scheme,where.netloc)==(issuer.scheme,issuer.netloc)
        assert parse_qs(where.query)["client_id"]==[config["client_id"]]
        clients.append(config["client_id"])
    assert clients[0]!=clients[1]
    user_id,session_id=str(uuid.uuid4()),str(uuid.uuid4())
    token,csrf=secrets.token_hex(32),secrets.token_hex(32)
    hashed=hashlib.sha256(token.encode()).hexdigest()
    sql(f"""INSERT INTO owners(owner_id) VALUES('{user_id}');
        INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES('{user_id}','platform-release-fixture','{user_id}','{user_id}');
        INSERT INTO wallets(owner_id) VALUES('{user_id}');
        INSERT INTO user_sessions(token_hash,session_id,user_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at)
        VALUES('{hashed}','{session_id}','{user_id}','{csrf}',0,floor(extract(epoch FROM clock_timestamp()))::bigint+600,
        floor(extract(epoch FROM clock_timestamp()))::bigint+600);""")
    cookie="__Host-orange-user="+token
    status,headers,body=request(user,"/api/v1/session",cookie)
    view=json.loads(body)
    assert status==200 and view["user_id"]==user_id and view["personal_owner_id"]==user_id and headers["Cache-Control"]=="no-store"
    assert request(admin,"/admin/v1/session",cookie)[0]==401
    sql(f"UPDATE user_sessions SET revoked=true WHERE session_id='{session_id}'")
    assert request(user,"/api/v1/session",cookie)[0]==401
    migrated=int(sql("SELECT count(*) FROM _sqlx_migrations WHERE success"))
    assert migrated==migration_count
    return migrated


def check(root, url_file, user_config, admin_config, scratch):
    common={"PATH":"/usr/bin:/bin","ORANGE_ENV":"development"}
    psql=shutil.which("psql") or "/opt/homebrew/opt/postgresql@17/bin/psql"
    with database(url_file,"orange_release_") as (_,url):
        db=urlsplit(url)
        pg=dict(os.environ,PGHOST=db.hostname,PGPORT=str(db.port or 5432),PGUSER=unquote(db.username or ""),
                PGPASSWORD=unquote(db.password or ""),PGDATABASE=unquote(db.path[1:]),PGCONNECT_TIMEOUT="5")
        def sql(s):
            return subprocess.check_output([psql,"-X","-w","-Atq","-v","ON_ERROR_STOP=1"],env=pg,input=s,text=True).strip()
        objects=scratch/"objects";objects.mkdir(mode=0o700)
        common["DATABASE_URL"]=url
        uenv=dict(common,ORANGE_USER_BIND="127.0.0.1:0",ORANGE_USER_CONFIG_FILE=str(user_config),ORANGE_ARTIFACT_ROOT=str(objects))
        aenv=dict(common,ORANGE_ADMIN_BIND="127.0.0.1:0",ORANGE_ADMIN_CONFIG_FILE=str(admin_config))
        with ExitStack() as group:
            user=group.enter_context(listening(root/"bin/orange-user-api",uenv,"User API"))
            admin=group.enter_context(listening(root/"bin/orange-admin-api",aenv,"Admin API"))
            migrated=check_running(user,admin,user_config,admin_config,request,sql,len(list((root/"migrations").glob("*.sql"))))
    return {"actual_user_api":True,"actual_admin_api":True,"oidc_clients_distinct":True,"session_fixture_revoked":True,
            "typed_auth_rejections":True,"server_assigned_diagnostic_ids":True,"anonymous_denied":True,"cross_surface_rejected":True,"successful_migrations":migrated,"graceful_stop":True}
