"""Build the actual historical gateway from the commit that shipped a given schema, entirely offline."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tarfile
import tempfile

RELEASES={50:"b6a542f7414851b5336adb86bb9a7fe6e12416c8",51:"ef97803d5db5f5f9d8324a81c80bf853069bfe3f"}


def digest(path):
    h=hashlib.sha256()
    with path.open("rb") as src:
        for buf in iter(lambda:src.read(1<<20),b""):h.update(buf)
    return h.hexdigest()


def prepare(root,version,cache=None):
    root=Path(root).resolve()
    commit=RELEASES[version]
    def git(*args):
        return subprocess.check_output(["git",*args],cwd=root)
    versions=[int(Path(n).name.split("_",1)[0]) for n in git("ls-tree","-r","--name-only",commit,"services/control-plane/migrations").decode().splitlines()]
    if max(versions)!=version or set(versions)!=set(range(1,version+1)):raise ValueError("Wrong historical migration set")
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
    rustc=subprocess.check_output([cargo.parent/"rustc","-vV"],text=True)
    identity={"source":commit,"schema":version,"rustc":rustc,"lock":hashlib.sha256(git("show",commit+":Cargo.lock")).hexdigest()}
    base=Path(cache) if cache else root/".local/historical-worker-gateways"
    base.mkdir(mode=0o700,parents=True,exist_ok=True)
    if base.is_symlink() or base.stat().st_uid!=os.getuid() or stat.S_IMODE(base.stat().st_mode)&0o077:raise ValueError("Expected a private historical build cache")
    key=hashlib.sha256(json.dumps(identity,sort_keys=True).encode()).hexdigest()
    dest=base/key
    binary=dest/"orange-worker-gateway"
    with (base/(key+".lock")).open("a") as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        if dest.exists():
            info=json.loads((dest/"build.json").read_bytes())
            if dest.is_symlink() or binary.is_symlink() or not binary.is_file() or info!={**identity,"sha256":digest(binary)}:
                raise ValueError("Retained historical executable differs from the locked receipt")
        else:
            with tempfile.TemporaryDirectory(dir=base,prefix=".build-") as name:
                work=Path(name);tree=work/"source";tree.mkdir()
                archive=work/"source.tar"
                with archive.open("xb") as stream:
                    subprocess.run(["git","archive",commit],cwd=root,stdout=stream,check=True)
                with tarfile.open(archive) as files:
                    entries=files.getmembers()
                    if any(not (m.isfile() or m.isdir()) or Path(m.name).is_absolute() or ".." in Path(m.name).parts for m in entries):raise ValueError("Unexpected historical Git archive member")
                    files.extractall(tree,members=entries)
                # Keep the source archive and target isolated: newer SQLx embeds a different migration set.
                env={k:v for k,v in os.environ.items() if k in {"HOME","PATH","TMPDIR","SDKROOT","DEVELOPER_DIR","CARGO_HOME","RUSTUP_HOME","PROTOC","LANG"}}
                env.update(PATH=str(cargo.parent)+os.pathsep+env["PATH"],CARGO_INCREMENTAL="0",CARGO_NET_OFFLINE="true",CARGO_BUILD_JOBS="2",CARGO_PROFILE_DEV_DEBUG="0",CARGO_TARGET_DIR=str(work/"target"))
                subprocess.run([cargo,"build","--locked","--offline","-p","orange-control-plane","--bin","orange-worker-gateway"],cwd=tree,env=env,check=True,timeout=1800)
                staging=work/"published";staging.mkdir(mode=0o700)
                exe=staging/binary.name;shutil.copy2(work/"target/debug"/binary.name,exe)
                info={**identity,"sha256":digest(exe)}
                (staging/"build.json").write_text(json.dumps(info,indent=2)+"\n")
                staging.rename(dest)
        print(json.dumps({"historical_gateway_schema":version,"source":commit,"executable_sha256":info["sha256"]}),flush=True)
        return binary.resolve()
