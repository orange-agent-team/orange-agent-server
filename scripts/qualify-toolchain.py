#!/usr/bin/env python3
"""Local/CI qualification of the four source checkouts using the same locked, offline build commands."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib

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


def clean_sources(root, dest, archive, commit):
    dest.mkdir()
    subprocess.run(["git","archive","--format=tar","-o",archive,commit],cwd=root,check=True)
    with tarfile.open(archive) as source:
        source.extractall(dest,filter="data")


def reproduce(roots,node):
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
                env.update(CARGO_TARGET_DIR=str(tree/"target"),CARGO_INCREMENTAL="0",CARGO_PROFILE_DEV_DEBUG="0")
                args=["--bin","orange-user-api"] if name=="server" else ["-p","orange-desktop-bridge","--lib"]
                subprocess.run([cargo,"build","--locked","--offline",*args],cwd=tree,env=env,check=True)
                binary=tree/"target/debug"/("orange-user-api" if name=="server" else "liborange_desktop_bridge.dylib")
                outcomes[name]={"artifact":binary.name,"sha256":sha(binary),"bytes":binary.stat().st_size,"cargo_lock_sha256":sha(tree/"Cargo.lock")}
                if name=="desktop":
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
    parser.add_argument("--section",required=True,choices=["inventory","pins","reproduce"])
    for key in ("desktop","web","admin"):
        parser.add_argument("--"+key,type=Path)
    parser.add_argument("--node",type=Path,default=Path.home()/".nvm/versions/node/v22.23.2/bin/node")
    args=parser.parse_args()
    if args.section=="pins":
        report=pins()
    else:
        roots={"server":ROOT}
        for kind in ("desktop","web","admin"):
            path=getattr(args,kind)
            require(path is not None,"Missing explicit "+kind+" checkout")
            roots[kind]=path.resolve()
        node=args.node.resolve()
        stack=inventory(roots,node)
        report=stack if args.section=="inventory" else {"stack":stack,"clean_build":reproduce(roots,node)}
    print(json.dumps(report,sort_keys=True),flush=True)


if __name__=="__main__":
    main()
