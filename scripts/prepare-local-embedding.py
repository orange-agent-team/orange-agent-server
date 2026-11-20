#!/usr/bin/env python3
"""Fetch fixed E5 assets into a private local model cache and verify their content."""

import fcntl
import hashlib
from http.client import IncompleteRead
import json
import os
from pathlib import Path
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode, urlsplit
from urllib.request import HTTPRedirectHandler, build_opener

ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / ".local/embedding-model"
PIN = json.loads((ROOT / "config/local-embedding.lock.json").read_text())


class PublisherRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        url = urlsplit(newurl)
        if (
            url.scheme != "https"
            or url.hostname not in ("modelscope.cn", "cdn-lfs-cn-1.modelscope.cn")
            or url.username
            or url.password
            or url.port not in (None, 443)
        ):
            raise ValueError("Unexpected publisher redirect")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def digest(stream):
    h = hashlib.sha256()
    for block in iter(lambda: stream.read(1024 * 1024), b""):
        h.update(block)
    return h.hexdigest()


def fetch(opener, name, spec):
    dest = CACHE / Path(name).name
    if dest.exists():
        with dest.open("rb") as handle:
            if (
                dest.is_symlink()
                or dest.stat().st_size != spec["bytes"]
                or digest(handle) != spec["sha256"]
            ):
                raise ValueError("Existing file differs from pinned asset")
        return
    query = urlencode({"Revision": PIN["revision"], "FilePath": name})
    url = "https://modelscope.cn/api/v1/models/" + PIN["repository"] + "/repo?" + query
    with tempfile.NamedTemporaryFile(dir=CACHE, delete=False) as tmp:
        staging = Path(tmp.name)
        try:
            with opener.open(url, timeout=45) as response:
                hashed = hashlib.sha256()
                remaining = spec["bytes"]
                while remaining:
                    block = response.read(min(1024 * 1024, remaining))
                    if not block:
                        raise EOFError("Incomplete publisher response")
                    tmp.write(block)
                    hashed.update(block)
                    remaining -= len(block)
                if response.read(1) or hashed.hexdigest() != spec["sha256"]:
                    raise ValueError("Publisher asset digest mismatch")
            tmp.flush()
            os.fsync(tmp.fileno())
            os.replace(staging, dest)
        finally:
            staging.unlink(missing_ok=True)


if __name__ == "__main__":
    os.umask(0o077)
    CACHE.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (CACHE / "fetch.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for name, spec in PIN["files"].items():
            for attempt in range(3):
                try:
                    fetch(build_opener(PublisherRedirect()), name, spec)
                    break
                except HTTPError as error:
                    if error.code not in (408, 429, 500, 502, 503, 504) or attempt == 2:
                        raise
                except (TimeoutError, URLError, EOFError, IncompleteRead):
                    if attempt == 2:
                        raise
                time.sleep(2**attempt)
            print("Verified", name, spec["sha256"], flush=True)
