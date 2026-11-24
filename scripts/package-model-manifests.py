#!/usr/bin/env python3.12
"""Produce and run the macOS ARM64 standalone ModelVersion inspection bundle, fully offline."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

from platform_payload import write_archive
from rust_notices import notices

ROOT=Path(__file__).resolve().parents[1]
PROGRAM="orange-model-manifest"
CACHES={"text":"model-runtime","image":"image-model","transcription":"transcription-model","embedding":"embedding-model"}


def git(*args):
    return subprocess.check_output(["git",*args],cwd=ROOT)


def sha(path):
    with path.open("rb") as data:return hashlib.file_digest(data,"sha256").hexdigest()


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--vendor",required=True,type=Path,help="Verified Cargo vendor tree")
    cli.add_argument("--asset-root",type=Path,default=ROOT/".local")
    args=cli.parse_args()
    if git("status","--porcelain"):raise ValueError("Clean committed source required")
    commit=git("rev-parse","HEAD").decode().strip()
    def source(name):return git("show",commit+":"+name)
    subprocess.run([sys.executable,ROOT/"scripts/build-embedding-release.py","--profile","model-manifest","--commit",commit],cwd=ROOT,check=True)
    native=ROOT/".local/model-manifest-release"/commit
    build=json.loads((native/"native-build.json").read_bytes())
    binary=native/PROGRAM
    binaries={name:sha(native/name) for name in [PROGRAM,"model-fetch"]}
    if build["source_commit"]!=commit or build["target"]!="aarch64-apple-darwin" or not build["reproducible"] or build["builds"]!=[binaries]*2:
        raise ValueError("Invalid native build receipts")
    files={"bin/"+PROGRAM:binary.read_bytes(),"README.md":source("distribution/model-manifest/README.md"),"docs/model-version-manifests.md":source("docs/model-version-manifests.md"),"Cargo.lock":source("Cargo.lock")}
    for name in ("contracts/json-schema/model-version.v1.json","contracts/json-schema/user.schema.json","contracts/openapi/model-manifests.v1.json"):
        files[name]=source(name)
    files["bin/model-fetch"]=(native/"model-fetch").read_bytes()
    files["config/local-model.lock.json"]=source("config/local-model.lock.json")
    versions={}
    for name in git("ls-tree","-r","--name-only",commit,"--","crates/model-runtime/model-manifests").decode().splitlines():
        doc=json.loads(source(name));id_=doc["model_version"]
        if Path(name).stem!=id_ or id_ in versions:raise ValueError("Ambiguous bundled version")
        versions[id_]=doc;files["models/"+Path(name).name]=source(name)
    total,missing=notices(files,args.vendor.resolve(strict=True),files["Cargo.lock"],source)
    if missing:raise ValueError("Missing Cargo notices")
    manifest={"source":commit,"target":build["target"],"sha256":{k:hashlib.sha256(v).hexdigest() for k,v in sorted(files.items())}}
    files["manifest.json"]=(json.dumps(manifest,sort_keys=True,indent=2)+"\n").encode()
    dest=ROOT/".local/model-manifest-distributions";dest.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(dir=dest) as tmp:
        work=Path(tmp);first=work/"first.tgz";second=work/"second.tgz"
        write_archive(first,files,prefix=PROGRAM);write_archive(second,files,prefix=PROGRAM)
        checksum=sha(first)
        if checksum!=sha(second):raise ValueError("Independent archive bytes differ")
        with tarfile.open(first) as arc:arc.extractall(work,filter="data")
        installed=work/PROGRAM
        for k,v in files.items():
            if (installed/k).read_bytes()!=v:raise ValueError("Installed member differs: "+k)
        def execute(*argv,data=None):return json.loads(subprocess.check_output([installed/"bin"/PROGRAM,*argv],input=data,timeout=360))
        ids=execute("list")
        if [k for k,_ in ids]!=sorted(versions):raise ValueError("Packaged catalogue differs from source")
        checks={};adapters={}
        for id_,digest in ids:
            raw=(installed/"models"/(id_+".json")).read_bytes()
            result=execute("validate",data=raw)
            if result!={"manifest":versions[id_],"manifest_sha256":digest} or result!=execute("show",id_):raise ValueError("Packaged version differs")
            kind=result["manifest"]["interface"]["modality"]
            if kind in ("text","embedding"):
                capability=execute("adapter",id_)
                if capability["manifest_sha256"]!=digest or capability["runtime"]!=result["manifest"]["runtime"] or capability["adapter"]["modality"]!=kind:
                    raise ValueError("Installed adapter is not bound to its version")
                adapters[kind]=capability["adapter"]
            elif subprocess.run([installed/"bin"/PROGRAM,"adapter",id_],capture_output=True).returncode==0:
                raise ValueError("Unsupported local HTTP capability was advertised")
            checks[kind]=execute("verify",id_,args.asset_root.resolve()/CACHES[kind])
            if checks[kind]["manifest_sha256"]!=digest or checks[kind]["verified_bytes"]!=result["manifest"]["resources"]["artifact_bytes"]:
                raise ValueError("Packaged verifier did not measure exact declared assets")
        if set(checks)!=set(CACHES):raise ValueError("Missing modality")
        import fcntl
        spec=installed/"config/local-model.lock.json";cache=args.asset_root.resolve()/CACHES["text"]
        spec_data=json.loads(spec.read_bytes())["model"];weight=cache/(spec_data["sha256"]+".gguf")
        def observe(directory):
            return json.loads(subprocess.check_output([installed/"bin/model-fetch",spec,directory,"--inspect"],timeout=180))
        absent=work/"absent";before=observe(absent)
        if before["state"]!="absent" or absent.exists():raise ValueError("Read-only inspection created a cache")
        with weight.open("rb") as reader:
            fcntl.flock(reader,fcntl.LOCK_SH)
            status=observe(cache)
        if status["state"]!="verified" or status["read_lease_observed"] is not True or status["sha256"]!=spec_data["sha256"] or status["expected_bytes"]!=spec_data["bytes"]:
            raise ValueError("Installed cache inspector did not verify the real locked artifact")
        final=dest/(PROGRAM+"-macos-arm64-"+checksum[:16]+".tar.gz")
        if final.exists():
            if final.is_symlink() or sha(final)!=checksum:raise ValueError("Conflicting release")
        else:os.link(first,final)
        report={"source":commit,"archive":final.name,"bytes":first.stat().st_size,"sha256":checksum,"native_builds":build["builds"],
                "identical_archives":2,"target":build["target"],"locked_crates":total,"missing_notices":missing,
                "measured":checks,"local_http_adapters":adapters,"observed_cache":status,"production_signed_or_notarized":False}
        receipt=work/"receipt.json";receipt.write_text(json.dumps(report,indent=2)+"\n")
        os.replace(receipt,final.with_suffix(".json"));print(json.dumps(report),flush=True)


if __name__=="__main__":main()
