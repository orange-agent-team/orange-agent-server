#!/usr/bin/env python3
"""Qualify the normal Rust MC-05 entry and its supervised, real local CPU backend."""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import struct
import subprocess
import tempfile
import time
import wave

ROOT = Path(__file__).resolve().parents[1]
PIN = json.loads((ROOT / "config/local-transcription.lock.json").read_text())
parser = argparse.ArgumentParser()
parser.add_argument("--models", type=Path, default=ROOT / ".local/transcription-model")
parser.add_argument(
    "--python", type=Path, default=ROOT / ".local/transcription-venv/bin/python"
)
args = parser.parse_args()
# Preserve the venv symlink, whose containing directory identifies the environment.
python = args.python.absolute()
models = args.models.resolve()
target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")).resolve()
sample = models / "jfk.wav"
rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
cargo = Path(
    subprocess.check_output(
        [rustup, "which", "--toolchain", "1.97.1", "cargo"], text=True
    ).strip()
)


def sparse_wav(path, n, hz=16000, channels=1):
    size = n * channels * 2
    with path.open("wb") as out:
        out.write(
            struct.pack(
                "<4sI4s4sIHHIIHH4sI",
                b"RIFF",
                36 + size,
                b"WAVE",
                b"fmt ",
                16,
                1,
                channels,
                hz,
                hz * channels * 2,
                channels * 2,
                16,
                b"data",
                size,
            )
        )
        out.truncate(size + 44)


def main():
    if not __debug__:
        raise SystemExit("Verification requires assertions")
    fixture = PIN["qualification_sample"]
    assert hashlib.sha256(sample.read_bytes()).hexdigest() == fixture["sha256"]
    with wave.open(str(sample), "rb") as wav:
        assert (
            wav.getnframes(),
            wav.getframerate(),
            wav.getnchannels(),
            wav.getsampwidth(),
        ) == (176000, 16000, 1, 2)
        pcm_sha = hashlib.sha256(wav.readframes(wav.getnframes())).hexdigest()
    env = dict(os.environ, PATH=str(cargo.parent) + os.pathsep + os.environ["PATH"])
    subprocess.run(
        [
            cargo,
            "build",
            "--locked",
            "--offline",
            "-p",
            "orange-model-runtime",
            "--bin",
            "orange-transcribe",
        ],
        cwd=ROOT,
        env=env,
        check=True,
    )
    binary = target / "debug/orange-transcribe"
    with tempfile.TemporaryDirectory(prefix="orange-asr-qa-") as tmp:
        scratch = Path(tmp)
        slot = scratch / "slot"
        slot.mkdir(mode=0o700)
        receipt = scratch / "measurement.json"

        def cmd(action, path, rest=(), assets=models):
            return [binary, action, python, assets, slot, path, *rest]

        def run(action, path=sample, rest=(), assets=models, expected=None):
            result = subprocess.run(
                cmd(action, path, rest, assets),
                capture_output=True,
                text=True,
                timeout=180,
                env={"PATH": "/usr/bin:/bin"},
            )
            if expected:
                assert (
                    result.returncode != 0
                    and not result.stdout
                    and expected in result.stderr
                ), (result.returncode, result.stderr[:200])
                return
            if result.returncode:
                raise RuntimeError("Owned CPU run failed: " + result.stderr[:250])
            return json.loads(result.stdout)

        measured = run("measure")
        usage = measured["measure"]
        assert (
            measured["audio_sha256"] == fixture["sha256"]
            and "transcript" not in measured
        )
        assert (
            usage["normalized_sha256"] == pcm_sha
            and usage["source_samples"] == usage["normalized_samples"] == 176000
            and usage["duration_ms"] == 11000
        )
        fixtures = json.loads(
            subprocess.check_output(
                [
                    python,
                    "-I",
                    "-B",
                    ROOT / "scripts/transcription-media-fixtures.py",
                    sample,
                    scratch,
                ],
                text=True,
            )
        )
        formats = {"wav": usage["duration_ms"]}
        for case in fixtures:
            obj = run("measure", scratch / case["file"])
            m = obj["measure"]
            assert (
                m["format"] == case["format"]
                and m["sample_rate"] == case["source_rate"]
                and m["source_samples"] == case["source_frames"]
                and obj["audio_sha256"] == case["sha256"]
            )
            assert (
                m["duration_ms"]
                == (case["source_frames"] * 1000 + case["source_rate"] - 1)
                // case["source_rate"]
            )
            formats[case["format"]] = m["duration_ms"]
        receipt.write_text(json.dumps(measured))
        output = run("confirm", rest=[receipt])
        assert output["measure"] == usage
        transcript = output["transcript"]
        assert (
            transcript["language"] == "en"
            and transcript["compute_type"] in ("int8", "int8_float32")
            and transcript["segments"]
        )
        words = re.findall("[a-z]+", transcript["text"].lower())
        assert (
            words
            == "and so my fellow americans ask not what your country can do for you ask what you can do for your country".split()
        ), words
        assert all(
            0 <= v["start_ms"] <= v["end_ms"] <= 11000 for v in transcript["segments"]
        )
        tampered = json.loads(receipt.read_text())
        tampered["measure"]["normalized_sha256"] = "0" * 64
        run("confirm", rest=[receipt, "zz"], expected="Invalid")
        wrong = scratch / "wrong.json"
        wrong.write_text(json.dumps(tampered))
        run("confirm", rest=[wrong, "en"], expected="Changed")
        malformed = scratch / "bad.wav"
        malformed.write_bytes(sample.read_bytes()[:-2])
        run("measure", malformed, expected="Invalid")
        playlist = scratch / "media.txt"
        playlist.write_text("#EXTM3U\nhttps://example.invalid/private.wav\n")
        run("measure", playlist, expected="Invalid")
        edge = scratch / "edge.wav"
        sparse_wav(edge, 600 * 16000)
        assert run("measure", edge)["measure"]["duration_ms"] == 600000
        sparse_wav(edge, 600 * 16000 + 1)
        run("measure", edge, expected="Limit")
        sparse_wav(edge, (100000000 - 44) // 4, 96000, 2)
        assert edge.stat().st_size == 100000000
        full = run("measure", edge)["measure"]
        assert (
            full["source_samples"] == (100000000 - 44) // 4
            and full["sample_rate"] == 96000
        )
        with edge.open("ab") as out:
            out.write(b"x")
        run("measure", edge, expected="Limit")
        sparse_wav(edge, 17)
        assert run("measure", edge)["measure"]["duration_ms"] == 2
        with (slot / "transcription.lock").open("rb") as held:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            run("measure", expected="Busy")
        stale = scratch / "changed-model"
        stale.mkdir(mode=0o700)
        for file in PIN["files"]:
            if file == "tokenizer.json":
                data = (models / file).read_bytes()
                (stale / file).write_bytes(b"!" + data[1:])
            else:
                try:
                    os.link(models / file, stale / file)
                except OSError:
                    shutil.copyfile(models / file, stale / file)
        run("confirm", rest=[receipt], assets=stale, expected="Unavailable")
        last = json.loads((slot / "last-observed.json").read_text())["attempt_id"]
        proc = subprocess.Popen(
            cmd("confirm", sample, [receipt, "en"]),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env={"PATH": "/usr/bin:/bin"},
        )
        try:
            deadline = time.monotonic() + 120
            while time.monotonic() < deadline and proc.poll() is None:
                state = json.loads((slot / "last-observed.json").read_text())
                if state["attempt_id"] != last and state["phase"] == "inference":
                    break
                time.sleep(0.005)
            else:
                raise RuntimeError(
                    "Actual CPU model did not reach inference before cancellation"
                )
            proc.send_signal(signal.SIGINT)
            out, err = proc.communicate(timeout=15)
            assert proc.returncode != 0 and not out and "Cancelled" in err
            try:
                os.kill(state["pid"], 0)
            except ProcessLookupError:
                pass
            else:
                raise RuntimeError("Owned Python process not reaped")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
        assert run("measure")["measure"] == usage
        # Freeze only this new session so that parent loss and recovery are observable
        # independently of the model's speed. A surviving worker must keep its lease.
        old_attempt = json.loads((slot / "last-observed.json").read_text())[
            "attempt_id"
        ]
        lost = subprocess.Popen(
            cmd("confirm", sample, [receipt, "en"]),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
            env={"PATH": "/usr/bin:/bin"},
        )
        group = lost.pid
        stopped = False
        try:
            deadline = time.monotonic() + 120
            while time.monotonic() < deadline and lost.poll() is None:
                observed = json.loads((slot / "last-observed.json").read_text())
                if (
                    observed["attempt_id"] != old_attempt
                    and observed["phase"] == "inference"
                ):
                    if (
                        os.getpgid(lost.pid) != group
                        or os.getpgid(observed["pid"]) != group
                    ):
                        raise RuntimeError("unexpected process group")
                    os.killpg(group, signal.SIGSTOP)
                    stopped = True
                    break
                time.sleep(0.005)
            else:
                raise RuntimeError("Model did not reach the host-loss barrier")
            lost.kill()
            lost.wait(timeout=10)
            run("measure", expected="Busy")
            with (slot / "transcription.lock").open("rb") as lease:
                try:
                    fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    pass
                else:
                    raise RuntimeError(
                        "host disappearance made a live CPU slot reusable"
                    )
            os.killpg(group, signal.SIGCONT)
            stopped = False
            deadline = time.monotonic() + 60
            with (slot / "transcription.lock").open("rb") as lease:
                while True:
                    try:
                        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        break
                    except BlockingIOError:
                        if time.monotonic() > deadline:
                            raise RuntimeError(
                                "surviving worker did not release the slot"
                            )
                        time.sleep(0.05)
            std, _ = lost.communicate(timeout=5)
            assert not std
        finally:
            if stopped:
                try:
                    os.killpg(group, signal.SIGCONT)
                except ProcessLookupError:
                    pass
            if lost.poll() is None:
                lost.kill()
                lost.wait()
        assert run("measure")["measure"] == usage
        report = {
            "scope": "Actual local CPU and recorded English speech; no remote pricing, GPU, signing or language coverage certification",
            "model_version": PIN["model_version"],
            "model_digest": PIN["files"]["model.bin"]["sha256"],
            "input_digest": measured["audio_sha256"],
            "measured": usage,
            "decoded_formats_ms": formats,
            "compute_type": transcript["compute_type"],
            "detected_language": transcript["language"],
            "segment_count": len(transcript["segments"]),
            "transcript_sha256": hashlib.sha256(
                transcript["text"].encode()
            ).hexdigest(),
            "reference_words_matched": True,
            "changed_measure_denied": True,
            "duration_limit_ms": 600000,
            "bytes_accepted": 100000000,
            "bytes_rejected": 100000001,
            "reaped_after_inference_cancel": True,
            "reacquired_slot": True,
            "orphan_held_slot_until_exit": True,
            "model_mismatch_denied": True,
            "unlisted_language_denied": True,
        }
        evidence = ROOT / ".local/transcription-evidence"
        evidence.mkdir(mode=0o700, parents=True, exist_ok=True)
        (evidence / "integration.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report))


if __name__ == "__main__":
    main()
