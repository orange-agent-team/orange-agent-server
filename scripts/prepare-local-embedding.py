#!/usr/bin/env python3
"""Prepare the pinned E5 model and tokenizer."""

import json
from pathlib import Path
from pinned_modelscope_assets import prepare

root = Path(__file__).resolve().parents[1]
if __name__ == "__main__":
    prepare(
        root / ".local/embedding-model",
        json.loads((root / "config/local-embedding.lock.json").read_text()),
    )
