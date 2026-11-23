#!/usr/bin/env python3
"""Verify the complete locked Cargo notice inventory inside an independently digest-pinned distribution."""
import argparse
import hashlib
import json
from pathlib import Path
import tarfile
import tomllib
import zipfile


def require(ok,msg):
    if not ok:raise ValueError(msg)


def verify(container,expected):
    with container.open("rb") as source:
        require(hashlib.file_digest(source,"sha256").hexdigest()==expected,"Wrong archive digest")
        source.seek(0)
        material={}
        def retain(name):
            return name in {"Cargo.lock","manifest.json"} or name.startswith("licenses/")
        if container.suffix==".zip":
            prefix="orange-worker/"
            with zipfile.ZipFile(source) as archive:
                for m in archive.infolist():
                    if not m.is_dir() and m.filename.startswith(prefix) and retain(m.filename[len(prefix):]):
                        name=m.filename[len(prefix):]
                        require(name not in material,"Duplicate notice or manifest")
                        material[name]=archive.read(m)
        else:
            prefix="orange-platform/"
            with tarfile.open(fileobj=source,mode="r|gz") as archive:
                for m in archive:
                    if m.isfile() and m.name.startswith(prefix) and retain(m.name[len(prefix):]):
                        name=m.name[len(prefix):]
                        require(name not in material,"Duplicate notice or manifest")
                        material[name]=archive.extractfile(m).read()
    read=material.__getitem__
    lock=tomllib.loads(read("Cargo.lock").decode())["package"]
    required={(r["name"],r["version"]):r["checksum"] for r in lock if r.get("source","").startswith("registry+")}
    manifest=json.loads(read("manifest.json"));inventory=json.loads(read("licenses/inventory.json"))
    for path in ("Cargo.lock","licenses/inventory.json"):
        require(hashlib.sha256(read(path)).hexdigest()==manifest["sha256"].get(path),"Release inventory differs from its manifest")
    require(not inventory["without_packaged_notice"],"Release reports missing notice files")
    claims={}
    for record in inventory["packages"]:
        key=record["name"],record["version"]
        require(key not in claims and required.get(key)==record["registry_checksum"],"Unbound Cargo provenance")
        docs=record["notice_files"]
        require(bool(docs),"Crate has no delivered notice")
        for p in docs:
            require(p.startswith("licenses/") and ".." not in Path(p).parts,"Unexpected notice name")
            require(hashlib.sha256(read(p)).hexdigest()==manifest["sha256"].get(p),"Notice digest differs")
        claims[key]=docs
    require(claims.keys()==required.keys(),"Incomplete dependency coverage")
    if ("asn1-rs-impl","0.2.0") in claims:
        require({Path(p).name for p in claims["asn1-rs-impl","0.2.0"]}=={"LICENSE-MIT","LICENSE-APACHE"},"Missing upstream ASN.1 release notices")
    print(json.dumps({"archive":container.name,"sha256":expected,"locked_registry_sources":len(claims),"missing":0}),flush=True)


if __name__=="__main__":
    cli=argparse.ArgumentParser();cli.add_argument("archive",type=Path);cli.add_argument("--sha256",required=True)
    args=cli.parse_args();verify(args.archive,args.sha256)
