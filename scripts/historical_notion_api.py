"""Build the pinned pre-Notion User API for the schema-47 migration acceptance only."""
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

REVISION="8ea1e3df71c3f7f90c32986cd564b5c637d13e21"
TREE="ac87eb89c3dc3603ea7cda80499988e79282de69"
SOURCE_ARCHIVE="83ea10d4e9fb85acb06ce6c70900cd374e8de22c10cfff382c1e02bf8f13e6f9"
CARGO_LOCK="21434865943337e64721dc9dd92081c389b4dbb9c0709caf7db992233084fa9e"


def sha(path):
    digest=hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda:stream.read(1024*1024),b""):digest.update(chunk)
    return digest.hexdigest()


def binary(root,cargo,source_archive=None):
    common=subprocess.check_output(["git","rev-parse","--path-format=absolute","--git-common-dir"],cwd=root,text=True).strip()
    cache=Path(common).parent/".local/schema47-producer"
    cache.mkdir(parents=True,mode=0o700,exist_ok=True)
    if cache.is_symlink() or cache.stat().st_uid!=os.geteuid() or cache.stat().st_mode&0o077:raise ValueError("Private historical cache required")
    env={k:v for k,v in os.environ.items() if not k.startswith(("ORANGE_","CARGO_","RUST","DATABASE","PG","STRIPE_"))}
    env.update(PATH=str(cargo.parent)+os.pathsep+env["PATH"],CARGO_NET_OFFLINE="true",CARGO_PROFILE_DEV_DEBUG="0",CARGO_INCREMENTAL="0",CARGO_BUILD_JOBS="2")
    version=subprocess.check_output([cargo.parent/"rustc","-vV"],cwd=root,text=True,env=env)
    if "release: 1.97.1" not in version:raise ValueError("Expected locked Rust toolchain")
    compiler=subprocess.check_output(["protoc","--version"],env=env,text=True).strip()
    if compiler!="libprotoc 36.2":raise ValueError("Expected reviewed Protobuf compiler")
    identity={"source":REVISION,"tree":TREE,"lock_sha256":CARGO_LOCK,"rustc":version,"protoc":compiler,"profile":"dev-no-debug-v1"}
    key=hashlib.sha256(json.dumps(identity,sort_keys=True).encode()).hexdigest()
    installed=cache/key
    with (cache/"build.lock").open("a") as gate:
        fcntl.flock(gate,fcntl.LOCK_EX)
        if not installed.exists():
            if source_archive is None:
                tree=subprocess.check_output(["git","rev-parse",REVISION+"^{tree}"],cwd=root,text=True).strip()
                if tree!=TREE:raise ValueError("Wrong historical source tree")
                archive=subprocess.check_output(["git","-c","tar.umask=0002","archive","--format=tar.gz",REVISION],cwd=root)
            else:
                archive=source_archive.read_bytes()
                if hashlib.sha256(archive).hexdigest()!=SOURCE_ARCHIVE:raise ValueError("Wrong exported historical source")
            with tempfile.TemporaryDirectory(dir=cache,prefix=".build-") as scratch:
                folder=Path(scratch);source=folder/"source";source.mkdir()
                with tarfile.open(fileobj=io.BytesIO(archive),mode="r:gz") as files:
                    if any(not(m.isfile() or m.isdir()) or Path(m.name).is_absolute() or ".." in Path(m.name).parts for m in files):raise ValueError("Unexpected historical source member")
                    files.extractall(source)
                if sha(source/"Cargo.lock")!=CARGO_LOCK:raise ValueError("Historical dependency lock differs")
                migrations=sorted((source/"services/control-plane/migrations").glob("*.sql"))
                if not migrations or int(migrations[-1].name.split("_")[0])!=47:raise ValueError("Historical API does not end at schema 47")
                env.update(CARGO_TARGET_DIR=str(folder/"target"),RUSTFLAGS="--remap-path-prefix="+str(folder)+"=/orange-legacy",ZERO_AR_DATE="1")
                subprocess.run([cargo,"build","--locked","--offline","-p","orange-control-plane","--bin","orange-user-api"],cwd=source,env=env,check=True)
                ready=folder/"ready";ready.mkdir(mode=0o700);exe=ready/"orange-user-api"
                shutil.copyfile(folder/"target/debug/orange-user-api",exe);exe.chmod(0o700)
                (ready/"build.json").write_text(json.dumps({"identity":identity,"sha256":sha(exe)},sort_keys=True)+"\n")
                ready.rename(installed)
        exe=installed/"orange-user-api";receipt=installed/"build.json"
        if installed.is_symlink() or exe.is_symlink() or receipt.is_symlink() or not exe.is_file() or not os.access(exe,os.X_OK):
            raise ValueError("Historical build cache changed")
        record=json.loads(receipt.read_bytes())
        if record["identity"]!=identity or sha(exe)!=record["sha256"]:raise ValueError("Historical binary digest differs")
        print("Verified schema-47 User API",REVISION,record["sha256"],flush=True)
        return exe
