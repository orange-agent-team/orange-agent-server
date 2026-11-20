"""One bounded CPU request per supervised process. stdin/stdout are private JSON."""

import hashlib
import json
import os
from pathlib import Path
import stat
import sys


class OversizedText(Exception):
    """At least one tokenized row exceeds the fixed model profile."""


def verified_bytes(root, name, spec):
    fd = os.open(root / name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_size != spec["bytes"]:
            raise ValueError("artifact")
        data = stream.read(spec["bytes"] + 1)
    if len(data) != spec["bytes"] or hashlib.sha256(data).hexdigest() != spec["sha256"]:
        raise ValueError("artifact")
    return data


def infer(root, profile, texts):
    import numpy as np
    import onnxruntime as ort
    from tokenizers import Tokenizer

    files = profile["files"]
    tokenizer = Tokenizer.from_buffer(
        verified_bytes(root, "tokenizer.json", files["onnx/tokenizer.json"])
    )
    tokenizer.no_padding()
    tokenizer.no_truncation()
    encodings = tokenizer.encode_batch(texts, add_special_tokens=True)
    lengths = [len(item.ids) for item in encodings]
    if max(lengths) > profile["max_tokens_per_text"]:
        raise OversizedText
    width = max(lengths)
    pad = tokenizer.token_to_id("<pad>")
    if pad is None:
        raise ValueError("pad")
    ids = np.full((len(texts), width), pad, dtype=np.int64)
    mask = np.zeros_like(ids)
    types = np.zeros_like(ids)
    for index, encoding in enumerate(encodings):
        length = lengths[index]
        ids[index, :length] = encoding.ids
        mask[index, :length] = 1
        types[index, :length] = encoding.type_ids

    options = ort.SessionOptions()
    options.intra_op_num_threads = 2
    options.inter_op_num_threads = 1
    options.log_severity_level = 4
    model = verified_bytes(root, "model.onnx", files["onnx/model.onnx"])
    session = ort.InferenceSession(
        model, sess_options=options, providers=["CPUExecutionProvider"]
    )
    if session.get_providers() != ["CPUExecutionProvider"]:
        raise ValueError("provider")
    inputs = {"input_ids": ids, "attention_mask": mask, "token_type_ids": types}
    names = {v.name for v in session.get_inputs()}
    if not {"input_ids", "attention_mask"} <= names <= inputs.keys():
        raise ValueError("input tensor")
    hidden = session.run(["last_hidden_state"], {name: inputs[name] for name in names})[
        0
    ]
    if hidden.shape != (len(texts), width, profile["dimensions"]):
        raise ValueError("output shape")
    pooled = np.sum(hidden * mask[:, :, None], axis=1) / np.array(lengths)[:, None]
    norms = np.linalg.norm(pooled, axis=1, keepdims=True)
    if not np.isfinite(norms).all() or (norms <= 0).any():
        raise ValueError("normalization")
    return {"vectors": (pooled / norms).astype(np.float32).tolist(), "lengths": lengths}


def main():
    frame = sys.stdin.buffer.read(262145)
    if len(frame) > 262144:
        raise ValueError("frame")
    request = json.loads(frame)
    texts = request["texts"]
    profile = request["profile"]
    if (
        not isinstance(texts, list)
        or not 1 <= len(texts) <= profile["max_batch"]
        or not all(isinstance(t, str) and t.strip() for t in texts)
    ):
        raise ValueError("input")
    return infer(Path(sys.argv[1]), profile, texts)


if __name__ == "__main__":
    try:
        result = main()
    except OversizedText:
        result = {"error": "text_too_long"}
    except Exception:
        result = {"error": "backend_unavailable"}
    print(json.dumps(result, allow_nan=False, separators=(",", ":")))
