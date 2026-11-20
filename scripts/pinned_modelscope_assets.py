#!/usr/bin/env python3
"""Fetch only hash-pinned files from a fixed publisher snapshot with bounded retries."""

import fcntl
import hashlib
from http.client import IncompleteRead
import stat
import os
from pathlib import Path
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode, urlsplit
from urllib.request import HTTPRedirectHandler, build_opener


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


def fetch(opener, cache, pin, name, spec):
    dest = cache / Path(name).name
    if dest.is_symlink():
        raise ValueError("Cache entries must be regular files")
    if dest.exists():
        fd = os.open(dest, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, "rb") as handle:
            info = os.fstat(fd)
            if (
                not stat.S_ISREG(info.st_mode)
                or info.st_size != spec["bytes"]
                or digest(handle) != spec["sha256"]
            ):
                raise ValueError("Existing file differs from pinned asset")
        return
    query = urlencode({"Revision": pin["revision"], "FilePath": name})
    url = "https://modelscope.cn/api/v1/models/" + pin["repository"] + "/repo?" + query
    with tempfile.NamedTemporaryFile(dir=cache, delete=False) as tmp:
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


def retry_download(operation, *args):
    for attempt in range(3):
        try:
            return operation(*args)
        except HTTPError as error:
            if error.code not in (408, 429, 500, 502, 503, 504) or attempt == 2:
                raise
        except (TimeoutError, URLError, EOFError, IncompleteRead):
            if attempt == 2:
                raise
        time.sleep(2**attempt)


def prepare(cache, pin):
    os.umask(0o077)
    cache.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (cache / "fetch.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for name, spec in pin["files"].items():
            retry_download(fetch, build_opener(PublisherRedirect()), cache, pin, name, spec)
            print("Verified", name, spec["sha256"], flush=True)
