#!/usr/bin/env python3
"""Cache pinned Linux formatting and linting components outside the offline contribution job."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import tempfile
import time
from urllib.parse import urlsplit
import urllib.request

ROOT=Path(__file__).resolve().parents[1]


def main():
    p=argparse.ArgumentParser()
    p.add_argument("--inputs",type=Path,default=ROOT/".local/linux")
    args=p.parse_args()
    args.inputs.mkdir(mode=0o700,parents=True,exist_ok=True)
    data=json.loads((ROOT/"config/ci-linux-tools.lock.json").read_bytes())
    for name,c in data["components"].items():
        source=urlsplit(c["url"])
        if source.scheme!="https" or source.netloc!="static.rust-lang.org" or not source.path.endswith(".tar.xz"):
            raise ValueError("Unexpected release origin")
        out=args.inputs/Path(source.path).name
        if out.exists():
            with out.open("rb") as stream:
                if hashlib.file_digest(stream,"sha256").hexdigest()==c["sha256"]:
                    print("PASS cached",name,c["sha256"])
                    continue
            raise ValueError("Existing CI input does not match its lock: "+name)
        for attempt in range(3):
            try:
                with tempfile.NamedTemporaryFile(dir=args.inputs,delete=False) as file:
                    tmp=Path(file.name)
                    try:
                        with urllib.request.urlopen(c["url"],timeout=25) as response:
                            digest=hashlib.sha256()
                            while chunk:=response.read(1024*1024):
                                if file.tell()+len(chunk)>100*1024*1024:
                                    raise ValueError("Oversized component archive")
                                file.write(chunk)
                                digest.update(chunk)
                        if digest.hexdigest()!=c["sha256"]:
                            raise ValueError("Incorrect component digest")
                        file.flush()
                        os.replace(tmp,out)
                    finally:
                        tmp.unlink(missing_ok=True)
                break
            except OSError:
                if attempt==2:raise
                time.sleep(2**attempt)
        print("PASS downloaded",name,c["sha256"])


if __name__=="__main__":
    main()
