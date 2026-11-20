#!/usr/bin/env python3
"""Prepare the fixed CT2 assets and verify the official recorded qualification sample."""

import base64
import fcntl
import hashlib
import json
import os
import re
import stat
from pathlib import Path
import tempfile
from urllib.request import HTTPRedirectHandler, Request, build_opener
from pinned_modelscope_assets import prepare, retry_download

root = Path(__file__).resolve().parents[1]
cache = root / ".local/transcription-model"
pin = json.loads((root / "config/local-transcription.lock.json").read_text())


class PinnedSampleEndpoint(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("Sample endpoint redirected away from the pinned API request")


def read_response(url, accept, bound):
    request = Request(url, headers={"Accept": accept, "User-Agent": "Orange-Agent-validation"})
    with build_opener(PinnedSampleEndpoint()).open(request, timeout=20) as response:
        data = response.read(bound + 1)
        if len(data) > bound:
            raise ValueError("Oversized sample response")
        return data


def download_audio(sample):
    if "git_blob" not in sample:
        return read_response(sample["url"], "audio/wav", sample["bytes"])
    url = f"https://api.github.com/repos/{sample['repository']}/contents/{sample['path']}?ref={sample['revision']}"
    obj = json.loads(read_response(url, "application/vnd.github+json", 1024 * 1024))
    audio = base64.b64decode(obj["content"].replace("\n", ""), validate=True)
    if (obj["sha"] != sample["git_blob"]
        or hashlib.sha1(f"blob {len(audio)}\0".encode() + audio).hexdigest() != sample["git_blob"]):
        raise ValueError("Sample Git blob mismatch")
    return audio


def ensure_sample(dest, sample):
    if not dest.exists() and not dest.is_symlink():
        audio = retry_download(download_audio, sample)
        if len(audio) != sample["bytes"] or hashlib.sha256(audio).hexdigest() != sample["sha256"]:
            raise ValueError("Sample identity mismatch")
        with tempfile.NamedTemporaryFile(dir=dest.parent, delete=False) as f:
            staging = Path(f.name)
            try:
                f.write(audio)
                f.flush()
                os.fsync(f.fileno())
                os.replace(staging, dest)
            finally:
                staging.unlink(missing_ok=True)
    fd = os.open(dest, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as source:
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_size != sample["bytes"]
            or hashlib.sha256(source.read(sample["bytes"] + 1)).hexdigest() != sample["sha256"]):
            raise ValueError("Sample digest mismatch")
    print("Verified fixed audio sample", dest.name, sample["sha256"])


if __name__ == "__main__":
    prepare(cache, pin)
    config = json.loads((cache / "config.json").read_bytes())
    tokenizer = json.loads((cache / "tokenizer.json").read_bytes())
    tokens = {x["id"]: x["content"] for x in tokenizer["added_tokens"]}
    codes = sorted(
        re.fullmatch(r"<\|([a-z]{2,3})\|>", tokens[n]).group(1)
        for n in config["lang_ids"]
    )
    if codes != pin["language_codes"] or len(set(codes)) != len(codes):
        raise ValueError(
            "Declared languages differ from the pinned model and tokenizer"
        )
    sample = pin["qualification_sample"]
    with (cache / "sample.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        ensure_sample(cache / "jfk.wav", sample)
        zh = json.loads((root / "config/transcription-qualification.json").read_text())["zh"]
        ensure_sample(cache / zh["filename"], zh)
