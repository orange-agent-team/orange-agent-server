"""Start the packaged Ubuntu APIs with private loopback-only PostgreSQL and Keycloak instances."""
from contextlib import ExitStack
import http.client
import io
import json
import os
from pathlib import Path
import secrets
import subprocess
import tarfile
import tempfile
import time
import uuid
from linux_platform_inputs import local_image_id
from platform_package_smoke import check_running

IDENTITY="sha256:3f3c5f8e03c614774b4bd9173b88391fefa4153bfc010d8012d59ad825377332"

def smoke(docker,package,lock,*,rotate_device=False):
    if not __debug__:raise RuntimeError("Verification assertions must be enabled")
    pg_image=local_image_id(docker,lock["test_database"])
    runtime=local_image_id(docker,lock["runtime_image"])
    if not pg_image or not runtime:raise ValueError("Pinned Ubuntu and PostgreSQL images required")
    actual=subprocess.check_output(docker+["image","inspect","--format","{{.Id}} {{.Os}}/{{.Architecture}}",IDENTITY],text=True).strip()
    if actual!=IDENTITY+" linux/arm64":raise ValueError("Verified local Keycloak 26.8.1 Linux/ARM64 fixture required")
    nonce=uuid.uuid4().hex
    def call(args,**kwargs):
        return subprocess.run(docker+args,timeout=kwargs.pop("timeout",45),check=True,**kwargs)
    def dispose(cid):
        own=subprocess.run(docker+["inspect","--format",'{{index .Config.Labels "orange.smoke"}}',cid],text=True,capture_output=True)
        if own.returncode==0 and own.stdout.strip()==nonce:call(["rm","-v","-f",cid],stdout=subprocess.DEVNULL)
    def create(group,name,args,environment=None):
        proc=call(["create","--label","orange.smoke="+nonce,"--name","orange-linux-"+name+"-"+nonce,
                   "--pull=never","--platform","linux/arm64","--pids-limit","512","--security-opt","no-new-privileges",*args],
                  env=environment,capture_output=True,text=True)
        cid=proc.stdout.strip()
        if len(cid)!=64 or not all(c in "0123456789abcdef" for c in cid):raise ValueError("Invalid smoke container ID")
        group.callback(dispose,cid);return cid
    def copy_data(cid,directory,files):
        archive=io.BytesIO()
        with tarfile.open(fileobj=archive,mode="w") as tar:
            for parent in sorted({str(Path(n).parent) for n in files}-{"."}):
                item=tarfile.TarInfo(parent);item.type=tarfile.DIRTYPE;item.mode=0o755;tar.addfile(item)
            for name,data in files.items():
                item=tarfile.TarInfo(name);item.size=len(data);item.mode=0o444
                tar.addfile(item,io.BytesIO(data))
        call(["cp","-",cid+":"+directory],input=archive.getvalue(),stdout=subprocess.DEVNULL)
    def ready(check,cid,label,limit):
        deadline=time.monotonic()+limit
        while time.monotonic()<deadline:
            if check():return
            running=call(["inspect","--format","{{.State.Running}}",cid],capture_output=True,text=True).stdout.strip()
            if running!="true":raise RuntimeError(label+" terminated before readiness")
            time.sleep(.75)
        raise TimeoutError(label+" did not become ready before its local deadline")
    user,admin=43110,43111
    with ExitStack() as group,tempfile.TemporaryDirectory(prefix="orange-linux-config-") as private:
        private=Path(private)
        password=secrets.token_hex(24)
        pg=create(group,"db",["--network","none","--cap-drop","ALL","--user","postgres","--memory","256m",
            "--tmpfs","/var/lib/postgresql/data:rw,uid=999,gid=999,mode=0700","-e","POSTGRES_PASSWORD","-e","POSTGRES_DB=orange_dev","-e","POSTGRES_INITDB_ARGS=--auth-host=scram-sha-256 --auth-local=trust",
            pg_image,"postgres","-c","listen_addresses=127.0.0.1","-c","shared_buffers=16MB"],
            dict(os.environ,POSTGRES_PASSWORD=password))
        call(["start",pg],stdout=subprocess.DEVNULL)
        def sql(statement):
            return call(["exec","-i","--user","postgres",pg,"psql","-X","-Atq","-v","ON_ERROR_STOP=1","-d","orange_dev"],
                input=statement,text=True,capture_output=True).stdout.strip()
        def db_ready():
            try:return sql("SELECT 1")=="1"
            except subprocess.CalledProcessError:return False
        ready(db_ready,pg,"PostgreSQL",90)
        namespace="container:"+pg
        def get_response(port,path,cookie=None):
            # All TCP stays inside this run's network namespace. Send the fixture cookie over stdin only.
            if not isinstance(port,int) or not 0<port<65536 or any(c in path for c in "\r\n"):raise ValueError("Invalid local request")
            buf=f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n"
            if cookie:buf+="Cookie: "+cookie+"\r\n"
            result=call(["exec","-i",pg,"bash","-c",'exec 3<>/dev/tcp/127.0.0.1/"$1" || exit 1; cat >&3; cat <&3',"request",str(port)],
                    input=(buf+"\r\n").encode(),capture_output=True,timeout=12)
            class Socket:
                def makefile(self,*_):return io.BytesIO(result.stdout)
            return decode_response(Socket())
        def http_ready(port,path,status):
            try:return get_response(port,path)[0]==status
            except (subprocess.CalledProcessError,http.client.HTTPException,OSError):return False
        issuer="http://127.0.0.1:18480/realms/orange-dev"
        clients=[];configs=[]
        for kind in ("user","admin"):
            conf=json.loads((package/"config"/(kind+"-oidc.example.json")).read_bytes())
            conf.update(issuer=issuer,client_secret=secrets.token_urlsafe(36))
            origin=conf[kind+"_origin"]
            clients.append({"clientId":conf["client_id"],"protocol":"openid-connect","secret":conf["client_secret"],"publicClient":False,
                            "enabled":True,"standardFlowEnabled":True,"redirectUris":[origin+"/*"],"webOrigins":[origin]})
            path=private/(kind+".json");path.write_text(json.dumps(conf));path.chmod(0o600);configs.append(path)
        idp=create(group,"identity",["--network",namespace,"--cap-drop","ALL","--memory","800m","--cpus","0.8",
                   "-e","JAVA_OPTS_KC_HEAP=-Xms128m -Xmx448m",IDENTITY,"start-dev","--http-host=127.0.0.1","--http-port=18480","--import-realm"])
        copy_data(idp,"/opt/keycloak/data",{"import/orange-dev-realm.json":json.dumps({"realm":"orange-dev","enabled":True,"sslRequired":"none","clients":clients}).encode()})
        call(["start",idp],stdout=subprocess.DEVNULL)
        ready(lambda:http_ready(18480,"/realms/orange-dev/.well-known/openid-configuration",200),idp,"OIDC",180)
        services=[]
        for kind,port,path,cfg in [("user",user,"/api/v1/session",configs[0]),("admin",admin,"/admin/v1/session",configs[1])]:
            key=kind.upper()
            cid=create(group,kind,["--network",namespace,"--cap-drop","ALL","--user","65532:65532","--memory","320m",
                *(["--tmpfs","/var/lib/orange-objects:rw,mode=0700,uid=65532,gid=65532","-e","ORANGE_ARTIFACT_ROOT=/var/lib/orange-objects"] if kind=="user" else []),
                "-e","ORANGE_ENV=development","-e","DATABASE_URL","-e",f"ORANGE_{key}_CONFIG_FILE=/opt/fixture.json",
                "-e",f"ORANGE_{key}_BIND=127.0.0.1:{port}","--entrypoint","/opt/orange-platform/bin/orange-"+kind+"-api",runtime],
                dict(os.environ,DATABASE_URL="postgres://postgres:"+password+"@127.0.0.1:5432/orange_dev"))
            call(["cp",str(package),cid+":/opt/"],stdout=subprocess.DEVNULL)
            copy_data(cid,"/opt",{"fixture.json":cfg.read_bytes()})
            call(["start",cid],stdout=subprocess.DEVNULL)
            ready(lambda:http_ready(port,path,401),cid,kind+" API",60)
            call(["exec","-w","/opt/orange-platform",cid,"sha256sum","--quiet","-c","SHA256SUMS"],capture_output=True)
            services.append(cid)
        migrated=check_running(user,admin,*configs,get_response,sql,len(list((package/"migrations").glob("*.sql"))))
        from linux_device_enrollment_smoke import check as enrolled_device
        helper=local_image_id(docker,lock["build_image"])
        if not helper:raise ValueError("Missing locked arm64 HTTP/2 fixture tools")
        device_check=enrolled_device(call,create,ready,group,package,helper,pg,namespace,"postgres://postgres:"+password+"@127.0.0.1:5432/orange_dev",
            user,json.loads(configs[0].read_text())["user_origin"],sql,rotate=rotate_device)
        # Execute all remaining Ubuntu ELF entry points without credentials and require a controlled refusal.
        from local_distribution_profiles import PLATFORM_PROGRAMS
        for program in PLATFORM_PROGRAMS:
            if program in ("orange-user-api","orange-admin-api"):continue
            res=subprocess.run(docker+["exec",services[0],"/usr/bin/env","-i","PATH=/usr/bin:/bin","/opt/orange-platform/bin/"+program],capture_output=True,timeout=15)
            if res.returncode!=1:raise RuntimeError(program+" failed to refuse unconfigured execution cleanly: "+str(res.returncode))
        for cid in services:
            call(["stop","--signal","SIGINT","--timeout","25",cid],stdout=subprocess.DEVNULL)
            if call(["inspect","--format","{{.State.ExitCode}}",cid],capture_output=True,text=True).stdout.strip()!="0":raise RuntimeError("Ungraceful API stop")
        return {"device_enrollment":device_check,"ubuntu":runtime,"postgres":pg_image,"keycloak":IDENTITY,"linux_arm64":True,"private_network":"loopback only",
                "actual_user_api":True,"actual_admin_api":True,"session_revocation":True,"independent_oidc_clients":True,
                "migrations":migrated,"all_six_executables_loaded":True,"public_ports":0}

def decode_response(sock):
    with http.client.HTTPResponse(sock) as res:
        res.begin()
        return res.status,res.headers,res.read(65537)
