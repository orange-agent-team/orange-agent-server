#!/usr/bin/env python3
"""Inventory the actually shipped wheels/native files and the frozen Cargo dependency graph."""
import argparse
from email.parser import BytesParser
from email.policy import compat32
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from zipfile import ZipFile

from local_distribution_profiles import PROFILES

ROOT = Path(__file__).resolve().parents[1]
MAX_MEMBER = 512*1024*1024
TARGET = "aarch64-apple-darwin"


def digest_stream(stream, sink=None):
    hashed = hashlib.sha256()
    for block in iter(lambda:stream.read(1024*1024),b""):
        hashed.update(block)
        if sink is not None: sink.write(block)
    return hashed.hexdigest()


def safe_name(name):
    path = PurePosixPath(name)
    if not name or path.is_absolute() or ".." in path.parts or "\\" in name:
        raise ValueError("Unexpected member name")
    return path


def inspect_wheel(stream, filename, sha):
    with ZipFile(stream) as package:
        files = [i for i in package.infolist() if not i.is_dir()]
        if len(files)>100000 or sum(f.file_size for f in files)>2*1024**3:
            raise ValueError("Wheel inventory limit exceeded")
        seen = set()
        licenses, binaries = {}, {}
        metadata = []
        embedded = []
        for f in files:
            name = safe_name(f.filename)
            if f.filename in seen or f.file_size>1024**3:
                raise ValueError("Duplicated or oversized wheel member")
            seen.add(f.filename)
            if name.name=="METADATA" and name.parent.name.endswith(".dist-info"):
                if f.file_size>4*1024**2:raise ValueError("Oversized wheel metadata")
                entry = package.read(f)
                if len(name.parts)==2:
                    metadata.append(entry)
                else:
                    inner=BytesParser(policy=compat32).parsebytes(entry)
                    embedded.append({"metadata_path":str(name),"name":inner.get("Name"),"version":inner.get("Version"),
                                     "license_expression":inner.get("License-Expression"),"metadata_sha256":hashlib.sha256(entry).hexdigest()})
            if (set(x.lower() for x in name.parts) & {"licenses","license","licences","licence"}
                or re.match(r"^(license|licence|copying|notice|copyright)([.\-_]|$)",name.name,re.I)):
                with package.open(f) as src:licenses[f.filename]=digest_stream(src)
            elif re.search(r"\.(dylib|so|pyd|dll|metallib)(\.\d+)*$",name.name,re.I):
                with package.open(f) as src:binaries[f.filename]=digest_stream(src)
        if len(metadata)!=1:raise ValueError("Expected a single top-level metadata document: "+filename)
        info=BytesParser(policy=compat32).parsebytes(metadata[0])
        package_name,version=info["Name"],info["Version"]
        if not package_name or not version:raise ValueError("Incomplete wheel identity")
        label=str(info.get("License") or "").strip()
        expression=info.get("License-Expression")
        classifiers=[v for v in info.get_all("Classifier",[]) if v.startswith("License ::")]
        row={"name":package_name,"version":version,"wheel":filename,"sha256":sha,"license_expression":expression,
             "license_field":label if len(label)<=160 else None,"classifiers":classifiers,
             "license_field_sha256":hashlib.sha256(label.encode()).hexdigest() if label else None,
             "declared_license_files":info.get_all("License-File",[]),"notice_files":licenses,"native_files":binaries,
             "project_urls":info.get_all("Project-URL",[]),"embedded_distributions":embedded}
        row["license_metadata_present"]=bool(expression or label or classifiers or licenses)
        return row


def inspect_archive(archive, profile, commit):
    observed={};wheels=[];manifest=None;cargo_lock=None;model=None;notices=[];total=0
    with tarfile.open(archive,mode="r|gz") as tar:
        for member in tar:
            path=safe_name(member.name)
            if (not member.isfile() or len(path.parts)<2 or path.parts[0]!="orange-"+profile
                or member.size>MAX_MEMBER or member.size<0):
                raise ValueError("Unexpected archive member")
            member_name=str(PurePosixPath(*path.parts[1:]))
            if member_name in observed:raise ValueError("Duplicate archive member")
            total+=member.size
            if total>4*1024**3:raise ValueError("Archive inventory limit exceeded")
            with tar.extractfile(member) as data:
                if member_name.startswith("wheels/") and member_name.endswith(".whl"):
                    with tempfile.SpooledTemporaryFile(max_size=2*1024**2) as snapshot:
                        sha=digest_stream(data,snapshot);snapshot.seek(0)
                        wheels.append(inspect_wheel(snapshot,path.name,sha))
                elif member_name in ("manifest.json","Cargo.lock","notices/sources.json",f"config/local-{profile}.lock.json"):
                    if member.size>8*1024**2:raise ValueError("Oversized manifest")
                    raw=data.read();sha=hashlib.sha256(raw).hexdigest()
                    if member_name=="manifest.json":manifest=json.loads(raw)
                    elif member_name=="Cargo.lock":cargo_lock=raw
                    elif member_name=="notices/sources.json":notices=json.loads(raw)
                    else:model=json.loads(raw)
                else:
                    sha=digest_stream(data)
            observed[member_name]=sha
    if (not manifest or not cargo_lock or not model or manifest["runtime_source"]!=commit
        or manifest["target"]!=TARGET):raise ValueError("Unrecognized archive identity")
    observed.pop("manifest.json")
    if manifest["sha256"]!=observed:raise ValueError("A packaged file differs from its manifest")
    if len({r["name"].lower().replace("_","-") for r in wheels})!=len(wheels):raise ValueError("Duplicate wheel distribution")
    installed={r["name"].lower().replace("_","-"):r for r in wheels}
    for record in notices:
        row=installed.get(record["package"])
        if row is None or row["version"]!=record["version"] or observed.get("notices/"+record["file"])!=record["sha256"]:
            raise ValueError("Unbound supplemental notice")
        row.setdefault("supplemental_notices",[]).append(record)
    return sorted(wheels,key=lambda r:r["name"].lower()),cargo_lock,model,observed


def cargo_inventory(commit, profile, cargo_lock):
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
    env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
    with tempfile.TemporaryDirectory(prefix="orange-sbom-src-") as temp:
        tree=Path(temp)
        with tempfile.TemporaryFile() as stream:
            subprocess.run(["git","archive",commit],cwd=ROOT,stdout=stream,check=True);stream.seek(0)
            with tarfile.open(fileobj=stream) as archive:
                archive.extractall(tree,filter="data")
        if (tree/"Cargo.lock").read_bytes()!=cargo_lock:raise ValueError("Cargo.lock differs from source")
        args=PROFILES[profile].cargo_args
        features=["--features",args[args.index("--features")+1]] if "--features" in args else []
        result=subprocess.check_output([cargo,"tree","--locked","--offline","-p","orange-model-runtime","--target",TARGET,
            "--edges","normal,build","--prefix","none","--format","{p}",*features],cwd=tree,env=env,text=True)
        selected=set()
        for line in result.splitlines():
            match=re.fullmatch(r"([A-Za-z0-9_-]+) v([^ ]+)(?: \(.*\))?(?: \(\*\))?",line)
            if not match:raise ValueError("Unexpected dependency row: "+line)
            selected.add(match.groups())
        metadata_features=["--features","orange-model-runtime/"+features[1]] if features else []
        meta=json.loads(subprocess.check_output([cargo,"metadata","--format-version","1","--offline","--locked",
            "--filter-platform",TARGET,*metadata_features],cwd=tree,env=env))
        lock={(r["name"],r["version"],r.get("source")):r for r in tomllib.loads(cargo_lock.decode())["package"]}
        rows=[]
        for package in meta["packages"]:
            key=(package["name"],package["version"])
            if key not in selected:continue
            source=package["source"]
            entry=lock[(*key,source)]
            license_path=package.get("license_file")
            notice=None
            if license_path:
                parent=Path(package["manifest_path"]).parent
                path=(parent/license_path).resolve()
                if not path.is_relative_to(parent.resolve()) or not path.is_file():
                    raise ValueError("Unresolved declared crate license file")
                with path.open("rb") as stream:notice={"path":str(path.relative_to(parent)),"sha256":digest_stream(stream)}
            rows.append({"name":key[0],"version":key[1],"source":source or "first-party workspace",
                         "checksum":entry.get("checksum"),"license":package.get("license"),"license_file":notice})
        row_ids={(r["name"],r["version"]) for r in rows}
        if len(row_ids)!=len(rows) or row_ids!=selected:
            raise ValueError("Cargo package identity mismatch: "+str(sorted(selected-row_ids)))
        return sorted(rows,key=lambda r:(r["name"],r["version"]))


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--receipt",required=True,type=Path)
    parser.add_argument("--output",required=True,type=Path)
    args=parser.parse_args()
    report=json.loads(args.receipt.read_bytes())
    profile=report.get("profile","embedding")
    commit=report["source"];sha=report["sha256"]
    if profile not in PROFILES or not re.fullmatch("[a-f0-9]{40}",commit) or not re.fullmatch("[a-f0-9]{64}",sha):
        raise ValueError("Invalid distribution identity")
    name=f"orange-{profile}-macos-arm64-{sha[:16]}.tar.gz"
    if report["archive"]!=name:raise ValueError("Invalid content-addressed archive name")
    archive=args.receipt.parent/name
    with archive.open("rb") as src:
        if archive.stat().st_size!=report["bytes"] or digest_stream(src)!=sha:raise ValueError("Archive hash mismatch")
    wheels,lock,model,observed=inspect_archive(archive,profile,commit)
    if report["offline_wheels"]!=len(wheels):raise ValueError("Incomplete wheel inventory")
    crates=cargo_inventory(commit,profile,lock)
    inventory={"schema":"orange-local-distribution-inventory-v1","profile":profile,"target":TARGET,
               "source":commit,"archive_sha256":sha,"archive":name,"wheels":wheels,"cargo":crates,
               "native_programs":{k:v for k,v in observed.items() if k.startswith("bin/")},
               "external_model":{"repository":model.get("repository"),"revision":model.get("revision"),"license":model.get("license")},
               "review":{"crates_without_license_metadata":[r["name"] for r in crates if not r["license"] and not r["license_file"]],
                         "wheels_without_license_metadata":[r["name"] for r in wheels if not r["license_metadata_present"]],
                         "wheels_without_notice_files":[r["name"] for r in wheels if not r["notice_files"]]},
               "scope":"Shipped wheels/files and Cargo normal/build resolution at the runtime source. A license declaration is recorded as published; the inventory does not certify legal clearance or redistributability."}
    data=(json.dumps(inventory,indent=2,sort_keys=True)+"\n").encode()
    args.output.parent.mkdir(parents=True,exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=args.output.parent,delete=False) as tmp:
        path=Path(tmp.name)
        try:
            tmp.write(data);tmp.flush();os.fsync(tmp.fileno())
            try:os.link(path,args.output)
            except FileExistsError:
                if args.output.is_symlink() or args.output.read_bytes()!=data:raise ValueError("Existing inventory differs")
        finally:path.unlink(missing_ok=True)
    print(json.dumps({"archive":name,"wheels":len(wheels),"cargo_packages":len(crates),"review":inventory["review"]}))


if __name__=="__main__":
    main()
