#!/usr/bin/env python3
"""Prepare the two-publisher pinned SDXL Diffusers layout without loading model code."""
from pathlib import Path
import fcntl
import hashlib
import json
import os
import stat
from urllib.request import build_opener
from pinned_modelscope_assets import fetch, retry_download, PublisherRedirect

ROOT = Path(__file__).resolve().parents[1]


def main():
    os.umask(0o077)
    cache = ROOT / ".local/image-model"
    cache.mkdir(mode=0o700, parents=True, exist_ok=True)
    pin = json.loads((ROOT / "config/local-image.lock.json").read_text())
    def private(folder):
        info = folder.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise ValueError("Private owner-controlled cache directory required")
    private(cache)
    with (cache / "fetch.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for name, spec in sorted(pin["files"].items(), key=lambda pair: pair[1]["bytes"]):
            relative = Path(name)
            if relative.is_absolute() or ".." in relative.parts or len(relative.parts) > 2:
                raise ValueError("Invalid approved model layout")
            folder = (cache / relative).parent
            folder.mkdir(mode=0o700, exist_ok=True)
            private(folder)
            retry_download(fetch, build_opener(PublisherRedirect()), folder, pin, name, spec)
            if "git_blob" in spec:
                data = (cache / relative).read_bytes()
                if hashlib.sha1(f"blob {len(data)}\0".encode()+data).hexdigest() != spec["git_blob"]:
                    raise ValueError("Upstream Git blob does not match " + name)
            print("Verified", name, spec["bytes"], spec["sha256"], flush=True)


if __name__ == "__main__":
    main()
