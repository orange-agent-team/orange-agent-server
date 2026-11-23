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
        if container.suffix==".zip":
            archive=zipfile.ZipFile(source);prefix="orange-worker/"
            read=lambda p:archive.read(prefix+p)
        else:
            archive=tarfile.open(fileobj=source,mode="r:gz");prefix="orange-platform/"
            members={m.name:m for m in archive.getmembers() if m.isfile()}
            # Stream each compressed file once, not a backwards seek for every crate.
            source.seek(0)
            with tarfile.open(fileobj=source,mode="r|gz") as ordered:
                material={}
                for m in ordered:
                    if m.isfile() and (m.name.startswith(prefix+"licenses/") or m.name in {prefix+"Cargo.lock",prefix+"manifest.json"}):
                        material[m.name]=ordered.extractfile(m).read()
            read=lambda p:material[prefix+p]
        with archive:
            lock=tomllib.loads(read("Cargo.lock").decode())["package"]
            required={(r["name"],r["version"]):r["checksum"] for r in lock if r.get("source","").startswith("registry+")}
            manifest=json.loads(read("manifest.json"));inventory=json.loads(read("licenses/inventory.json"))
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
