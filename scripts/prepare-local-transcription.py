#!/usr/bin/env python3
"""Prepare the fixed CT2 assets and verify the official recorded qualification sample."""

import base64
import fcntl
import hashlib
import json
import os
import re
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


def read_sample_document(url):
    request = Request(url, headers={
        "Accept": "application/vnd.github+json",
        "User-Agent": "Orange-Agent-validation",
    })
    with build_opener(PinnedSampleEndpoint()).open(request, timeout=20) as response:
        return response.read(1024 * 1024 + 1)


def ensure_sample(cache, sample):
    dest = cache / "jfk.wav"
    if not dest.exists():
        url = f"https://api.github.com/repos/{sample['repository']}/contents/{sample['path']}?ref={sample['revision']}"
        doc = retry_download(read_sample_document, url)
        if len(doc) > 1024 * 1024:
            raise ValueError("Unexpected sample response")
        obj = json.loads(doc)
        audio = base64.b64decode(obj["content"].replace("\n", ""), validate=True)
        if (
            len(audio) != sample["bytes"]
            or obj["sha"] != sample["git_blob"]
            or hashlib.sha1(f"blob {len(audio)}\0".encode() + audio).hexdigest()
            != sample["git_blob"]
            or hashlib.sha256(audio).hexdigest() != sample["sha256"]
        ):
            raise ValueError("Sample identity mismatch")
        with tempfile.NamedTemporaryFile(dir=cache, delete=False) as f:
            staging = Path(f.name)
            try:
                f.write(audio)
                f.flush()
                os.fsync(f.fileno())
                os.replace(staging, dest)
            finally:
                staging.unlink(missing_ok=True)
    if (
        dest.is_symlink()
        or dest.stat().st_size != sample["bytes"]
        or hashlib.sha256(dest.read_bytes()).hexdigest() != sample["sha256"]
    ):
        raise ValueError("Sample digest mismatch")


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
        ensure_sample(cache, sample)
    print("Verified fixed audio sample", sample["sha256"])
