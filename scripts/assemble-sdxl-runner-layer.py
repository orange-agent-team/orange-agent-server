#!/usr/bin/env python3.12
"""Replace only the reviewed runner in the previously qualified immutable AMD64 image."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[1]
BASE_IMAGE = "sha256:b01f7baed816b97294e079f8a77493e0049d70c8c553cffff183b696a642fd33"
BASE_SOURCE = "f8882a6d2d8b4f94f6e64fff092e85aacd1894c8"


def require(ok, reason):
    if not ok:
        raise ValueError(reason)


def checksum(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def layer(runner):
    data=io.BytesIO()
    with tarfile.open(fileobj=data,mode="w",format=tarfile.USTAR_FORMAT) as archive:
        entry=tarfile.TarInfo("opt/orange/app/runner.py")
        entry.mode=0o644
        entry.size=len(runner)
        archive.addfile(entry,io.BytesIO(runner))
    raw=data.getvalue()
    zipped=io.BytesIO()
    with gzip.GzipFile(fileobj=zipped,mode="wb",mtime=0,filename="") as stream:
        stream.write(raw)
    compressed=zipped.getvalue()
    return compressed,{"diff_id":"sha256:"+hashlib.sha256(raw).hexdigest(),"layer":"sha256:"+hashlib.sha256(compressed).hexdigest(),
                       "bytes":len(compressed),"runner_sha256":hashlib.sha256(runner).hexdigest()}


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--base",type=Path,required=True,help="Qualified parent layout directory containing receipt.json and layout")
    parser.add_argument("--output",type=Path,default=ROOT/".local/sdxl-oci")
    args=parser.parse_args()
    parent=json.loads((args.base/"receipt.json").read_bytes())
    require(parent["image"]==BASE_IMAGE and parent["source"]==BASE_SOURCE,"Different sealed Debian image")
    source=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    require(not subprocess.check_output(["git","status","--porcelain"],cwd=ROOT),"Build from a clean committed checkout")
    origin=args.base/"layout/blobs/sha256"
    layout=args.output.resolve()/source/"layout"
    blobs=layout/"blobs/sha256"
    blobs.mkdir(parents=True,exist_ok=True)
    def verified(key):
        path=origin/key[7:]
        require(not path.is_symlink() and checksum(path)==key[7:],"Parent image digest changed")
        return path
    def put(data,kind):
        digest=hashlib.sha256(data).hexdigest()
        path=blobs/digest
        if not path.exists():
            with path.open("xb") as stream:stream.write(data)
        require(checksum(path)==digest,"Different generated blob")
        return {"digest":"sha256:"+digest,"size":len(data),"mediaType":kind}
    manifest=json.loads(verified(BASE_IMAGE).read_bytes())
    require(manifest["config"]["digest"]==parent["config_digest"],"Parent config differs")
    cfg=json.loads(verified(parent["config_digest"]).read_bytes())
    for desc in manifest["layers"]:
        src=verified(desc["digest"])
        require(src.stat().st_size==desc["size"],"Parent blob size changed")
        target=blobs/src.name
        if not target.exists():
            try:os.link(src,target)
            except OSError:shutil.copyfile(src,target)
        require(checksum(target)==src.name,"Failed to retain base layer")
    runner=subprocess.check_output(["git","show",source+":distribution/sdxl-linux/runner.py"],cwd=ROOT)
    first,report=layer(runner)
    second,check=layer(runner)
    require(first==second and report==check,"Independent runner layers differ")
    addition=put(first,"application/vnd.oci.image.layer.v1.tar+gzip")
    cfg["rootfs"]["diff_ids"].append(report["diff_id"])
    cfg["config"]["Labels"]["org.opencontainers.image.revision"]=source
    cfg["history"].append({"created":"2000-01-01T00:00:00Z","created_by":"Orange namespace-aware read-only weight verification"})
    config=put(encoded(cfg),"application/vnd.oci.image.config.v1+json")
    manifest["config"]=config
    manifest["layers"].append(addition)
    final=put(encoded(manifest),"application/vnd.oci.image.manifest.v1+json")
    tag="orange-dev-input/sdxl-cuda:"+source[:12]
    (layout/"oci-layout").write_bytes(encoded({"imageLayoutVersion":"1.0.0"}))
    (layout/"index.json").write_bytes(encoded({"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{**final,
          "platform":{"os":"linux","architecture":"amd64"},"annotations":{"org.opencontainers.image.ref.name":tag}}]}))
    receipt={**parent,"source":source,"previous_runner_source":BASE_SOURCE,"previous_runner_image":BASE_IMAGE,"image":final["digest"],
             "config_digest":config["digest"],"local_tag":tag,"runner_installations":[report,check]}
    (layout.parent/"receipt.json").write_bytes(encoded(receipt)+b"\n")
    print(json.dumps(receipt),flush=True)


if __name__=="__main__":
    main()
