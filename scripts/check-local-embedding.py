#!/usr/bin/env python3
"""Exercise the real private MP-03 HTTP endpoint and the normal Rust client."""

import errno
import http.client
import json
import os
from pathlib import Path
import selectors
import shutil
import socket
import subprocess
from concurrent.futures import ThreadPoolExecutor
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PIN = json.loads((ROOT / "config/local-embedding.lock.json").read_text())
MODELS = ROOT / ".local/embedding-model"
PYTHON = ROOT / "backends/.venv/bin/python"
CARGO = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
CARGO = Path(
    subprocess.check_output(
        [CARGO, "which", "--toolchain", "1.97.1", "cargo"], text=True
    ).strip()
)
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")).resolve()
ENV = dict(os.environ, PATH=str(CARGO.parent) + os.pathsep + os.environ["PATH"])
PROFILE = PIN["model_version"]


def request(session, data, token=None, path="/v1/embeddings"):
    conn = http.client.HTTPConnection("localhost", timeout=100)
    conn.sock = socket.socket(socket.AF_UNIX)
    conn.sock.settimeout(100)
    conn.sock.connect(session["socket"])
    try:
        conn.request(
            "POST",
            path,
            json.dumps(data),
            {
                "Authorization": "Bearer "
                + (session["token"] if token is None else token),
                "Content-Type": "application/json",
            },
        )
        response = conn.getresponse()
        return response.status, json.loads(response.read())
    finally:
        conn.close()


def client(session, texts):
    data = {"socket": session["socket"], "token": session["token"], "texts": texts}
    result = subprocess.run(
        [TARGET / "debug/examples/local-embeddings"],
        input=json.dumps(data),
        capture_output=True,
        text=True,
        check=True,
        timeout=100,
    )
    return json.loads(result.stdout)


def similarity(a, b):
    return sum(x * y for x, y in zip(a, b))


def check(session):
    texts = [
        "query: What is the capital of Japan?",
        "passage: The capital of Japan is Tokyo.",
        "passage: Baking bread requires yeast.",
    ]
    payload = {"model": PROFILE, "input": texts}
    for data, status, token in [
        (payload, 401, "invalid"),
        (dict(payload, model="unapproved"), 400, None),
        (dict(payload, dimensions=128), 400, None),
        (dict(payload, encoding_format="base64"), 400, None),
        (dict(payload, input=texts * 11), 413, None),
        (dict(payload, input=[]), 400, None),
        (dict(payload, unknown_tool="x"), 422, None),
        (dict(payload, input=[texts[0], "cat " * 513]), 422, None),
    ]:
        code, body = request(session, data, token)
        assert code == status and "data" not in body and "error" in body, (code, status)
    # This fixed tokenizer maps each standalone 'cat' to one token, plus two boundary tokens.
    edge_text = " ".join(["cat"] * 507)
    code, edge = request(session, dict(payload, input=edge_text))
    assert code == 200 and edge["usage"]["prompt_tokens"] == 512
    code, overflow = request(session, dict(payload, input=[texts[0], edge_text + " "]))
    assert code == 422 and "data" not in overflow
    chinese = client(
        session, ["query: 日本的首都是哪里？", "passage: 日本的首都是东京。"]
    )
    assert (
        len(chinese["data"]) == 2
        and chinese["data"][0]["embedding"] != chinese["data"][1]["embedding"]
    )

    def count(values):
        code, receipt = request(
            session, dict(payload, input=values), path="/v1/embeddings/input_tokens"
        )
        assert (
            code == 200
            and "data" not in receipt
            and len(receipt["projection_digest"]) == 64
        )
        assert receipt["input_tokens"] == sum(receipt["item_tokens"])
        return receipt

    projected = count(["hello", "passage: 东京"])
    assert projected == count(["query: hello", "passage: 东京"])
    assert (
        projected["projection_digest"]
        != count(["passage: hello", "passage: 东京"])["projection_digest"]
    )
    assert count([edge_text])["item_tokens"] == [512]
    first = client(session, texts)
    rows = [r["embedding"] for r in first["data"]]
    assert len(rows) == 3 and all(len(v) == 384 for v in rows)
    assert all(abs(similarity(v, v) - 1) < 0.0001 for v in rows)
    assert similarity(rows[0], rows[1]) > similarity(rows[0], rows[2])
    batch = [texts[i % 3] for i in range(32)]
    with ThreadPoolExecutor(max_workers=2) as pool:
        futures = [
            pool.submit(request, session, dict(payload, input=batch)) for _ in range(2)
        ]
        results = [f.result(timeout=100) for f in futures]
    assert sorted(r[0] for r in results) == [200, 429]
    complete = next(body for code, body in results if code == 200)
    assert [row["index"] for row in complete["data"]] == list(range(32))
    assert all(
        similarity(row["embedding"], rows[i % 3]) > 0.99999
        for i, row in enumerate(complete["data"])
    )
    single = [request(session, dict(payload, input=text))[1] for text in texts]
    counts = [r["usage"]["prompt_tokens"] for r in single]
    trusted_count = count(batch)
    assert trusted_count["item_tokens"] == [counts[i % 3] for i in range(32)]
    assert complete["usage"] == {
        "prompt_tokens": sum(counts[i % 3] for i in range(32)),
        "total_tokens": sum(counts[i % 3] for i in range(32)),
    }
    return {
        "dimensions": 384,
        "max_batch": 32,
        "single_counts": counts,
        "preflight_tokens": trusted_count["input_tokens"],
        "projection_digest": trusted_count["projection_digest"],
        "batch_usage": complete["usage"],
        "cosine_related": similarity(rows[0], rows[1]),
        "cosine_unrelated": similarity(rows[0], rows[2]),
        "accepted_tokens_per_text": 512,
        "rejected_tokens_per_text": 513,
        "unicode_rows": len(chinese["data"]),
        "errors_return_no_rows": True,
        "busy_status": 429,
        "model": PROFILE,
    }


def main():
    if not __debug__:
        raise SystemExit("Verification requires Python assertions")
    if not PYTHON.is_file() or not (MODELS / "model.onnx").is_file():
        raise SystemExit(
            "Install the locked embedding extra and verify the two pinned model assets first"
        )
    subprocess.run(
        [
            CARGO,
            "build",
            "--offline",
            "--locked",
            "-p",
            "orange-model-runtime",
            "--features",
            "embedding-host",
            "--bin",
            "orange-embedding-host",
            "--example",
            "local-embeddings",
        ],
        cwd=ROOT,
        env=ENV,
        check=True,
    )
    with tempfile.TemporaryDirectory(prefix="oe-", dir="/tmp") as scratch:
        assets = Path(scratch) / "assets"
        assets.mkdir(mode=0o700)
        # Copy the tokenizer because this test deliberately damages its private copy.
        shutil.copyfile(MODELS / "tokenizer.json", assets / "tokenizer.json")
        try:
            os.link(MODELS / "model.onnx", assets / "model.onnx")
        except OSError as error:
            if error.errno != errno.EXDEV:
                raise
            shutil.copyfile(MODELS / "model.onnx", assets / "model.onnx")
        proc = subprocess.Popen(
            [TARGET / "debug/orange-embedding-host", PYTHON, assets],
            env={"TMPDIR": scratch, "PATH": "/usr/bin:/bin"},
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
        )
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(proc.stdout, selectors.EVENT_READ)
                if not selector.select(20):
                    raise RuntimeError("local host did not listen")
            session = json.loads(proc.stdout.readline())
            assert session["phase"] == "listening" and session["model"] == PROFILE
            report = check(session)
            with (assets / "tokenizer.json").open("r+b") as tampered:
                tampered.write(b"!")
            status, denied = request(
                session, {"model": PROFILE, "input": ["query: probe"]}
            )
            assert status == 503 and "error" in denied and "data" not in denied
            report["same_length_digest_mismatch_denied"] = True
        finally:
            proc.stdin.close()
            try:
                proc.wait(timeout=100)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
                raise
        assert proc.returncode == 0 and not Path(session["socket"]).exists()
    report.update(
        clean_shutdown=True,
        source_revision=PIN["revision"],
        assets=PIN["files"],
        scope="Actual local CPU, private Unix HTTP, unbilled development",
    )
    (MODELS / "integration.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))


if __name__ == "__main__":
    main()
