"""Resolve the executable and immutable input prerequisites of the documented local first run."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tomllib


class ToolIssue(RuntimeError):
    def __init__(self,tool,remedy):
        self.tool=tool
        self.remedy=remedy
        super().__init__(remedy)


def version(cmd,label=None):
    name=label or Path(cmd[0]).name
    try:
        r=subprocess.run(cmd,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,check=True,timeout=20)
        return r.stdout.strip().splitlines()[0]
    except (OSError,subprocess.SubprocessError,IndexError):
        raise ToolIssue(name,"Check the locked executable or local resource named in the first-run guide") from None


def doctor(root):
    if sys.version_info[:2]!=(3,12):
        raise ToolIssue("python","Use Python 3.12 for this profile")
    channel=tomllib.loads((root/"rust-toolchain.toml").read_text())["toolchain"]["channel"]
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    rustc=version([rustup,"which","--toolchain",channel,"rustc"])
    stable=version([rustup,"which","--toolchain","stable","rustc"])
    compiler=version([rustc,"--version"])
    if not compiler.startswith("rustc "+channel+" ") or version([stable,"--version"])!=compiler:
        raise ToolIssue("rustup","Select the installed pinned 1.97.1 compiler for the server and Worker checks")
    protoc=version([os.environ.get("PROTOC","protoc"),"--version"],"protoc")
    if protoc!=json.loads((root/"crates/protocol/generation.lock.json").read_bytes())["compiler"]:
        raise ToolIssue("protoc","Set PROTOC to the locked 36.2 executable")
    pg_root=Path(os.environ.get("ORANGE_POSTGRES_BIN") or (Path(shutil.which("pg_ctl")).parent if shutil.which("pg_ctl") else "/opt/homebrew/opt/postgresql@17/bin"))
    pg=version([pg_root/"pg_ctl","--version"])
    if " 17." not in pg:
        raise ToolIssue("postgres","Set ORANGE_POSTGRES_BIN to the PostgreSQL 17 bin directory")
    for binary in ("psql","pg_ctl","initdb","createdb"):
        if not (pg_root/binary).is_file():
            raise ToolIssue(binary,"Supply the complete PostgreSQL 17 bin directory")
    ssl_path="/opt/homebrew/opt/openssl@3/bin/openssl"
    ssl=version([ssl_path if Path(ssl_path).exists() else "openssl","version"])
    if not ssl.startswith("OpenSSL 3."):
        raise ToolIssue("openssl","Use OpenSSL 3 for the temporary Worker CA")
    docker=shutil.which("docker") or "/usr/local/bin/docker"
    host=[docker,"--context","colima-orange-agent"]
    engine=version([*host,"info","--format","{{.ServerVersion}} {{.OSType}} {{.Architecture}}"])
    if " linux " not in engine:
        raise ToolIssue("docker","Start the dedicated colima-orange-agent Linux engine")
    from importlib.util import spec_from_file_location,module_from_spec
    spec=spec_from_file_location("local_dev_identity",root/"scripts/local-identity.py")
    module=module_from_spec(spec)
    spec.loader.exec_module(module)
    image=version([*host,"image","inspect","--format","{{.Id}}",module.IMAGE],"keycloak-image")
    if not image.startswith("sha256:"):
        raise ToolIssue("keycloak-image","Load the Keycloak image by its immutable digest")
    if sys.platform!="darwin" or os.uname().machine!="arm64":
        raise ToolIssue("platform","This qualified first-run profile requires macOS ARM64")
    sdk=version(["xcrun","--sdk","macosx","--show-sdk-version"])
    cc=version([version(["xcrun","--find","clang"]),"--version"])
    return {"macos_sdk":sdk,"c_compiler":cc,"python":sys.version.split()[0],"rustc":compiler,"protoc":protoc,"postgres":pg,"openssl":ssl,"git":version(["git","--version"]),
            "dedicated_docker":engine,"keycloak_ref":module.IMAGE,"imported_identity_image":image,
            "cargo_dependency_source":"previously acquired crates validated by Cargo.lock; no online resolution during rehearsal"}
