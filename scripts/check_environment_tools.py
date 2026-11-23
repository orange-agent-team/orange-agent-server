"""Resolve the executable and immutable input prerequisites of the documented local first run."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tomllib


def version(cmd):
    return subprocess.check_output(cmd,text=True,stderr=subprocess.STDOUT,timeout=20).strip().splitlines()[0]


def doctor(root):
    if sys.version_info[:2]!=(3,12):
        raise ValueError("This first-run profile uses Python 3.12")
    channel=tomllib.loads((root/"rust-toolchain.toml").read_text())["toolchain"]["channel"]
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    rustc=version([rustup,"which","--toolchain",channel,"rustc"])
    stable=version([rustup,"which","--toolchain","stable","rustc"])
    compiler=version([rustc,"--version"])
    if not compiler.startswith("rustc "+channel+" ") or version([stable,"--version"])!=compiler:
        raise ValueError("Pinned server Rust compiler required for the Worker fixture")
    protoc=version([os.environ.get("PROTOC","protoc"),"--version"])
    if protoc!=json.loads((root/"crates/protocol/generation.lock.json").read_bytes())["compiler"]:
        raise ValueError("Reinstall the pinned Protobuf compiler")
    pg_root=Path(os.environ.get("ORANGE_POSTGRES_BIN") or (Path(shutil.which("pg_ctl")).parent if shutil.which("pg_ctl") else "/opt/homebrew/opt/postgresql@17/bin"))
    pg=version([pg_root/"pg_ctl","--version"])
    if " 17." not in pg:
        raise ValueError("The managed cluster profile uses PostgreSQL 17")
    for binary in ("psql","pg_ctl","initdb","createdb"):
        if not (pg_root/binary).is_file():
            raise ValueError("Missing PostgreSQL binary "+binary)
    ssl_path="/opt/homebrew/opt/openssl@3/bin/openssl"
    ssl=version([ssl_path if Path(ssl_path).exists() else "openssl","version"])
    if not ssl.startswith("OpenSSL 3."):
        raise ValueError("Worker test certificates require OpenSSL 3")
    docker=shutil.which("docker") or "/usr/local/bin/docker"
    host=[docker,"--context","colima-orange-agent"]
    engine=version([*host,"info","--format","{{.ServerVersion}} {{.OSType}} {{.Architecture}}"])
    if " linux " not in engine:
        raise ValueError("Expected the dedicated Linux Docker engine")
    from importlib.util import spec_from_file_location,module_from_spec
    spec=spec_from_file_location("local_dev_identity",root/"scripts/local-identity.py")
    module=module_from_spec(spec)
    spec.loader.exec_module(module)
    image=version([*host,"image","inspect","--format","{{.Id}}",module.IMAGE])
    if not image.startswith("sha256:"):
        raise ValueError("Missing pinned local identity image")
    return {"python":sys.version.split()[0],"rustc":compiler,"protoc":protoc,"postgres":pg,"openssl":ssl,"git":version(["git","--version"]),
            "dedicated_docker":engine,"keycloak_ref":module.IMAGE,"imported_identity_image":image,
            "cargo_dependency_source":"previously acquired crates validated by Cargo.lock; no online resolution during rehearsal"}
