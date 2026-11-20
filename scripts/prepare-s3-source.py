#!/usr/bin/env python3
"""Install the pinned SeaweedFS executable for private, real-source macOS acceptance."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import tarfile
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, build_opener

root = Path(__file__).resolve().parents[1]
pin = json.loads((root / "config/s3-fixture.lock.json").read_text())
cache = root / ".local/seaweedfs"
archive = cache / "darwin_arm64.tar.gz"
binary = cache / "weed"

def digest(path):
    hashed = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024*1024), b""):
            hashed.update(block)
    return hashed.hexdigest()

class Redirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        url = urlsplit(newurl)
        if url.scheme!="https" or url.hostname not in ("github.com","release-assets.githubusercontent.com","objects.githubusercontent.com") or url.port not in (None,443) or url.username or url.password:
            raise ValueError("Unexpected release asset redirect")
        return super().redirect_request(req,fp,code,msg,headers,newurl)

def install(stream, path, size, expected, mode):
    hashed = hashlib.sha256()
    with tempfile.NamedTemporaryFile(dir=cache,delete=False) as tmp:
        part=Path(tmp.name)
        try:
            remaining=size
            while remaining:
                block=stream.read(min(remaining,1024*1024))
                if not block:raise EOFError("Incomplete fixture release")
                tmp.write(block)
                hashed.update(block)
                remaining-=len(block)
            if stream.read(1) or hashed.hexdigest()!=expected:
                raise ValueError("Fixture release digest mismatch")
            tmp.flush()
            os.fsync(tmp.fileno())
            part.chmod(mode)
            os.replace(part,path)
        finally:
            part.unlink(missing_ok=True)

if platform.system()!="Darwin" or platform.machine()!="arm64":
    raise SystemExit("Fixture release requires macOS arm64")
os.umask(0o077)
cache.mkdir(parents=True,exist_ok=True,mode=0o700)
with (cache/"prepare.lock").open("a") as lock:
    fcntl.flock(lock,fcntl.LOCK_EX)
    if binary.exists():
        if binary.is_symlink() or digest(binary)!=pin["weed_sha256"]:
            raise ValueError("Existing executable does not match the approved fixture")
    else:
        if not archive.exists():
            for attempt in range(3):
                try:
                    with build_opener(Redirect()).open(pin["asset"],timeout=45) as stream:
                        install(stream,archive,pin["bytes"],pin["sha256"],0o600)
                    break
                except HTTPError as error:
                    if error.code not in (408,429,500,502,503,504) or attempt==2:raise
                except (TimeoutError,URLError,EOFError):
                    if attempt==2:raise
                time.sleep(2**attempt)
        if archive.is_symlink() or archive.stat().st_size!=pin["bytes"] or digest(archive)!=pin["sha256"]:
            raise ValueError("Unexpected fixture archive")
        with tarfile.open(archive) as src:
            entry=src.getmember("weed")
            if not entry.isfile() or entry.size!=147006642:raise ValueError("Unexpected executable")
            with src.extractfile(entry) as stream:install(stream,binary,entry.size,pin["weed_sha256"],0o700)
    print("Verified SeaweedFS",pin["release"],pin["weed_sha256"])
