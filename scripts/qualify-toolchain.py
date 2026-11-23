#!/usr/bin/env python3
"""Local/CI qualification of the four source checkouts using the same locked, offline build commands."""
import argparse
import ctypes
from email.parser import BytesParser
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from urllib.parse import urlsplit
from zipfile import ZipFile

from linux_platform_inputs import local_image_id
from http_metrics_smoke import check_programs, ROLES

ROOT=Path(__file__).resolve().parents[1]
TARGET="aarch64-apple-darwin"
CRATES={
    "server":{"axum","opendal","opentelemetry","opentelemetry_sdk","opentelemetry-prometheus","prometheus","prost","sqlx","tokio","tonic","tonic-prost-build","tower"},
    "desktop":{"flutter_rust_bridge","rusqlite","tokio"},
}
FRONTEND={"react","react-dom","react-router","@tanstack/react-query","antd","vite","typescript"}


def require(ok, reason):
    if not ok:
        raise ValueError(reason)


def load(path):
    return json.loads(path.read_bytes())


def output(cmd,root,env=None):
    return subprocess.check_output(cmd,cwd=root,env=env,text=True).strip()


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream,"sha256").hexdigest()


def rust(root,version):
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(output([rustup,"which","--toolchain",version,"cargo"],root))
    env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"],CARGO_NET_OFFLINE="true")
    compiler=output([cargo.parent/"rustc","--version","--verbose"],root,env)
    require("release: "+version+"\n" in compiler and "host: "+TARGET in compiler,"Unqualified Rust target or version")
    return cargo,env


def rust_inventory(root,name,version):
    cargo,env=rust(root,version)
    graph=json.loads(output([cargo,"metadata","--format-version","1","--locked","--offline","--all-features","--filter-platform",TARGET],root,env))
    locked={(v["name"],v["version"]):v for v in tomllib.loads((root/"Cargo.lock").read_text())["package"]}
    records={}
    for package in graph["packages"]:
        key=package["name"]
        if key in CRATES[name]:
            lock=locked[key,package["version"]]
            require(re.fullmatch("[0-9a-f]{64}",lock["checksum"]) and package["license"],"Missing dependency identity: "+key)
            require(key not in records,"Multiple versions for foundational crate: "+key)
            records[key]={"version":package["version"],"license":package["license"],"sha256":lock["checksum"]}
    require(records.keys()==CRATES[name],"Incomplete resolved Rust stack")
    return {"compiler":version,"target":TARGET,"cargo_lock_sha256":sha(root/"Cargo.lock"),"components":records}


def frontend(root,kind):
    app=root/"apps"/kind
    data=load(app/"package-lock.json")["packages"]
    result={}
    pkg=load(app/"package.json")
    declared={**pkg["dependencies"],**pkg["devDependencies"]}
    require(FRONTEND <= declared.keys(),"Incomplete frontend stack")
    for name,version in declared.items():
        entry=data["node_modules/"+name]
        require(version == entry["version"],"Frontend dependency must match exact locked version: "+name)
        require(entry.get("integrity","").startswith("sha512-") and entry.get("license"),"Unidentified frontend package: "+name)
        result[name]={k:entry[k] for k in ("version","license","integrity")}
    return {"lock_sha256":sha(app/"package-lock.json"),"components":result}


def inventory(roots,node):
    desk=roots["desktop"]
    bridge=load(desk/"config/bridge-tools.lock.json")
    flutter=load(desk/"config/flutter.lock.json")
    rust_lock=tomllib.loads((ROOT/"rust-toolchain.toml").read_text())["toolchain"]["channel"]
    data={name:rust_inventory(roots[name],name,version) for name,version in [("server",rust_lock),("desktop",bridge["rust"])]}
    for kind in ("web","admin"):
        data[kind]=frontend(roots[kind],kind)
    require(output([node,"--version"],ROOT)=="v22.23.2","Unexpected Node")
    sdk=desk/".local/toolchains"/("flutter-"+flutter["version"])/"flutter"
    flutter_env=dict(os.environ,FLUTTER_SUPPRESS_ANALYTICS="true",DASH__SUPPRESS_ANALYTICS="true",
                     XDG_CONFIG_HOME=str(desk/".local/flutter-config"))
    actual=json.loads(output([sdk/"bin/flutter","--version","--machine"],desk,flutter_env))
    for live,pin in [("frameworkVersion","version"),("frameworkRevision","framework_revision"),("dartSdkVersion","dart_version")]:
        require(actual[live]==flutter[pin],"Flutter SDK differs: "+live)
    for binary,version in [(desk/".local/tools"/("frb-"+bridge["flutter_rust_bridge"])/"bin/flutter_rust_bridge_codegen",bridge["flutter_rust_bridge"]),
                           (desk/".local/tools"/("cargo-expand-"+bridge["cargo_expand"])/"bin/cargo-expand",bridge["cargo_expand"])]:
        require(output([binary,"--version"],desk).split()[-1]==version,"Generator version differs")
    require(output([os.getenv("PROTOC","protoc"),"--version"],ROOT)==load(ROOT/"crates/protocol/generation.lock.json")["compiler"],"Wrong protoc")
    data["native_tools"]={"flutter":flutter["version"],"flutter_revision":flutter["framework_revision"],"dart":flutter["dart_version"],
                          "frb_codegen":bridge["flutter_rust_bridge"],"cargo_expand":bridge["cargo_expand"],"node":output([node,"--version"],ROOT),
                          "protoc":output([os.getenv("PROTOC","protoc"),"--version"],ROOT)}
    return data


def pins():
    cfg=ROOT/"config"
    linux=load(cfg/"linux-platform-arm64.lock.json")
    images={}
    for key in ("build_image","runtime_image","test_database"):
        item=linux[key]
        require(re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}",item["image"]),"Image requires a content digest")
        for field in ("config_digest","manifest_list"):
            require(re.fullmatch(r"sha256:[0-9a-f]{64}",item[field]),"Incomplete OCI image provenance")
        images[key]=item["image"]
    models={}
    for name in ("local-model","local-image","local-embedding","local-transcription"):
        doc=load(cfg/(name+".lock.json"))
        model=doc.get("model",doc)
        require(re.fullmatch(r"[0-9a-f]{40}",model["revision"]) and model["license"],"Unqualified model revision")
        components=model.get("files",{model.get("file","weights"):model})
        for file,part in components.items():
            require(part["bytes"]>0 and re.fullmatch(r"[0-9a-f]{64}",part["sha256"]), "Missing asset digest "+file)
        models[name]={"revision":model["revision"],"license":model["license"],"components":len(components),"lock_sha256":sha(cfg/(name+".lock.json"))}
    engine=load(cfg/"local-model.lock.json")["backend"]
    require(re.fullmatch(r"[0-9a-f]{40}",engine["commit"]) and engine["license"],"Local engine is not pinned")
    return {"images":images,"models":models,"llama_cpp":engine["commit"]}


def installed_inputs(cache):
    locks=pins()
    cache=cache.resolve(strict=True)
    docker=[shutil.which("docker") or "/usr/local/bin/docker","--context","colima-orange-agent"]
    platform=load(ROOT/"config/linux-platform-arm64.lock.json")
    images={}
    for name in locks["images"]:
        identity=local_image_id(docker,platform[name])
        require(identity is not None,"Pinned local Linux/ARM64 image unavailable: "+name)
        images[name]=identity
    groups={"local-model":"model-runtime","local-embedding":"embedding-model","local-image":"image-model","local-transcription":"transcription-model"}
    assets={}
    for name,directory in groups.items():
        spec=load(ROOT/"config"/(name+".lock.json"))
        model=spec.get("model",spec)
        files=model.get("files",{model.get("file","weights"):model})
        for key,entry in files.items():
            relative=entry["sha256"]+".gguf" if name=="local-model" else Path(key).name if name=="local-embedding" else key
            path=(cache/directory/relative).resolve(strict=True)
            require(path.is_relative_to(cache/directory) and path.is_file() and path.stat().st_size==entry["bytes"]
                    and sha(path)==entry["sha256"],"Cached model differs from pinned input: "+name+"/"+key)
            assets[name+"/"+key]=entry["sha256"]
    receipt=load(cache/"model-runtime/build.json")
    binary=cache/"model-runtime/build/bin/llama-server"
    require(receipt["backend_commit"]==locks["llama_cpp"] and sha(binary)==receipt["executable_sha256"],
            "Cached llama.cpp executable differs from its qualified build")
    wheels={}
    for component,group,lockfile in [("diffusers","image","environments/sdxl-macos/uv.lock"),
                                     ("faster-whisper","transcription","uv.lock"),("onnxruntime","embedding","uv.lock")]:
        packages=tomllib.loads((ROOT/"backends"/lockfile).read_text())["package"]
        (package,)= [p for p in packages if p["name"]==component]
        entries={Path(urlsplit(w["url"]).path).name:w for w in package["wheels"]}
        wheel_dir=cache/(group+"-wheelhouse")
        (path,)=[f for f in wheel_dir.glob("*.whl") if f.name in entries]
        entry=entries[path.name]
        require(path.stat().st_size==entry["size"] and "sha256:"+sha(path)==entry["hash"],"Unqualified wheel: "+component)
        with ZipFile(path) as wheel:
            (meta,)=[n for n in wheel.namelist() if n.endswith(".dist-info/METADATA")]
            info=BytesParser().parsebytes(wheel.read(meta),headersonly=True)
        require(info["Version"]==package["version"],"Wheel version mismatch")
        licenses=[x for x in info.get_all("Classifier",[]) if x.startswith("License :: ")]
        license_name=info.get("License-Expression") or info.get("License") or ", ".join(licenses)
        require(bool(license_name),"Missing Python package license: "+component)
        wheels[component]={"artifact":path.name,"version":package["version"],"sha256":entry["hash"],"license":license_name}
    return {"images":images,"model_files":assets,"llama_server_sha256":receipt["executable_sha256"],"backend_wheels":wheels}


def clean_sources(root, dest, archive, commit):
    dest.mkdir()
    subprocess.run(["git","archive","--format=tar","-o",archive,commit],cwd=root,check=True)
    with tarfile.open(archive) as source:
        source.extractall(dest,filter="data")


def upgrade(desktop):
    commit=output(["git","rev-parse","HEAD"],desktop)
    with tempfile.TemporaryDirectory(prefix="orange-client-upgrade-") as work:
        tree=Path(work)/"desktop"
        clean_sources(desktop,tree,Path(work)/"desktop.tar",commit)
        local=tree/".local"
        local.mkdir()
        # Provision the same installations and dependency cache used by desktop CI.
        for name in ("tools","toolchains","pub-cache"):
            (local/name).symlink_to((desktop/".local"/name).resolve(strict=True),target_is_directory=True)
        lock=tree/"config/generated-bridge.lock.json"
        pinned=load(lock)
        f=load(tree/"config/flutter.lock.json")
        flutter=local/"toolchains"/("flutter-"+f["version"])/"flutter/bin/flutter"
        env=dict(os.environ,CARGO_NET_OFFLINE="true",CARGO_INCREMENTAL="0",CARGO_TARGET_DIR=str(tree/"target"),
                 PUB_CACHE=str(local/"pub-cache"),FLUTTER_SUPPRESS_ANALYTICS="true",DASH__SUPPRESS_ANALYTICS="true",
                 XDG_CONFIG_HOME=str(local/"flutter-config"),CARGO_PROFILE_DEV_DEBUG="0")
        subprocess.run([flutter,"pub","get","--offline","--enforce-lockfile"],cwd=tree/"apps/desktop",env=env,check=True)
        subprocess.run([sys.executable,tree/"scripts/generate-bridge.py"],cwd=tree,env=env,check=True)
        require(load(lock)==pinned,"Regenerated desktop API or generator input differs from the committed lock")
        subprocess.run([sys.executable,tree/"scripts/check-bridge-surface.py"],cwd=tree,env=env,check=True)
    subprocess.run([sys.executable,ROOT/"scripts/check-wire-reproducibility.py"],cwd=ROOT,check=True)
    return {"desktop_source":commit,"content_hash":pinned["content_hash"],"generator_inputs":len(pinned["source_sha256"]),
            "generated_files":len(pinned["generated_sha256"]),"wire_generation_rechecked":True}


def reproduce(roots,node,private):
    sources={k:output(["git","rev-parse","HEAD"],v) for k,v in roots.items()}
    outcomes={}
    with tempfile.TemporaryDirectory(prefix="orange-offline-ci-") as work:
        area=Path(work)
        for name,root in roots.items():
            tree=area/name
            clean_sources(root,tree,area/(name+".tar"),sources[name])
            if name in CRATES:
                version=(tomllib.loads((tree/"rust-toolchain.toml").read_text())["toolchain"]["channel"] if name=="server"
                         else load(tree/"config/bridge-tools.lock.json")["rust"])
                cargo,env=rust(tree,version)
                env.update(CARGO_TARGET_DIR=str(tree/"target"),CARGO_INCREMENTAL="0",CARGO_PROFILE_DEV_DEBUG="0",CARGO_PROFILE_DEV_STRIP="none")
                args=["--bin","orange-user-api"] if name=="server" else ["-p","orange-desktop-bridge","--lib"]
                subprocess.run([cargo,"build","--locked","--offline",*args],cwd=tree,env=env,check=True)
                binary=tree/"target/debug"/("orange-user-api" if name=="server" else "liborange_desktop_bridge.dylib")
                outcomes[name]={"artifact":binary.name,"sha256":sha(binary),"bytes":binary.stat().st_size,"cargo_lock_sha256":sha(tree/"Cargo.lock")}
                if name=="server":
                    roles=[item for item in ROLES if item[0]=="user"]
                    outcomes[name]["private_api_smoke"]=check_programs(binary.parent,private/"database-url",private/"identity/user-config.json",
                                                                        private/"identity/admin-config.json",roles)
                if name=="desktop":
                    library=ctypes.CDLL(str(binary))
                    function=library.frb_get_rust_content_hash
                    function.argtypes=[]
                    function.restype=ctypes.c_int32
                    actual=function()
                    require(str(actual)==str(load(tree/"config/generated-bridge.lock.json")["content_hash"]),"Native bridge ABI mismatch")
                    outcomes[name]["loaded_bridge_hash"]=actual
                    f=load(tree/"config/flutter.lock.json")
                    tool=root/".local/toolchains"/("flutter-"+f["version"])/"flutter/bin/flutter"
                    env.update(PUB_CACHE=str(root/".local/pub-cache"),FLUTTER_SUPPRESS_ANALYTICS="true",DASH__SUPPRESS_ANALYTICS="true")
                    app=tree/"apps/desktop"
                    subprocess.run([tool,"pub","get","--offline","--enforce-lockfile"],cwd=app,env=env,check=True)
                    subprocess.run([tool,"test","--no-pub","test/privacy_budget_test.dart"],cwd=app,env=env,check=True)
                    require(sha(app/"pubspec.lock")==sha(root/"apps/desktop/pubspec.lock"),"Dart lock drift")
                    outcomes[name]["flutter_privacy_budget_tests"]=True
            else:
                app=tree/"apps"/name
                env=dict(os.environ,PATH=str(node.parent)+os.pathsep+os.environ["PATH"],npm_config_cache=str(root/".local/npm-cache"),npm_config_offline="true")
                npm=node.parent/"npm"
                subprocess.run([npm,"ci","--offline","--no-audit","--no-fund"],cwd=app,env=env,check=True)
                subprocess.run([npm,"run","build"],cwd=app,env=env,check=True)
                require(sha(app/"package-lock.json")==sha(root/"apps"/name/"package-lock.json"),"npm lock drift")
                files={str(p.relative_to(app/"dist")):sha(p) for p in sorted((app/"dist").rglob("*")) if p.is_file()}
                require("index.html" in files and len(files)>1,"Missing frontend bundle")
                outcomes[name]={"files":files}
    return {"sources":sources,"outputs":outcomes}


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--section",required=True,choices=["inventory","pins","inputs","upgrade","reproduce"])
    for key in ("desktop","web","admin"):
        parser.add_argument("--"+key,type=Path)
    parser.add_argument("--node",type=Path,default=Path.home()/".nvm/versions/node/v22.23.2/bin/node")
    parser.add_argument("--asset-cache",type=Path,default=ROOT/".local")
    args=parser.parse_args()
    if args.section=="pins":
        report=pins()
    elif args.section=="inputs":
        report=installed_inputs(args.asset_cache)
    elif args.section=="upgrade":
        require(args.desktop is not None,"Supply a desktop checkout")
        report=upgrade(args.desktop.resolve())
    else:
        roots={"server":ROOT}
        for kind in ("desktop","web","admin"):
            path=getattr(args,kind)
            require(path is not None,"Missing explicit "+kind+" checkout")
            roots[kind]=path.resolve()
        node=args.node.resolve()
        stack=inventory(roots,node)
        report=stack if args.section=="inventory" else {"stack":stack,"clean_build":reproduce(roots,node,args.asset_cache.resolve())}
    print(json.dumps(report,sort_keys=True),flush=True)


if __name__=="__main__":
    main()
