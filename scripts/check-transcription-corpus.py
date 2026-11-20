#!/usr/bin/env python3
"""Qualify two reproducible 60-second speech fixtures and exact digital silence on macOS."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import shlex
import subprocess
import sys
import tempfile
import time
import wave

ROOT = Path(__file__).resolve().parents[1]
CORPUS = json.loads((ROOT / "config/transcription-qualification.json").read_text())
MODEL = json.loads((ROOT / "config/local-transcription.lock.json").read_text())
CACHE = ROOT / ".local/transcription-model"
RATE = CORPUS["sample_rate"]
FRAMES = CORPUS["duration_samples"]


def require(ok, message):
    if not ok:
        raise ValueError(message)


def make_fixture(folder, language):
    spec = CORPUS["zh"] if language == "zh" else MODEL["qualification_sample"]
    source = CACHE / spec.get("filename", "jfk.wav")
    require(hashlib.sha256(source.read_bytes()).hexdigest() == spec["sha256"], "Unrecognized source recording")
    with wave.open(str(source)) as stream:
        require((stream.getnchannels(), stream.getsampwidth(), stream.getframerate(), stream.getnframes())
                == (1, 2, RATE, spec["samples"]), "Source PCM layout changed")
        raw = stream.readframes(spec["samples"])
    pattern = raw + bytes(2 * CORPUS["silence_between_samples"])
    repeats, tail = divmod(2 * FRAMES, len(pattern))
    write_wav(folder / (language + ".wav"), repeats * pattern + bytes(tail))
    return repeats


def write_wav(path, data):
    with wave.open(str(path), "wb") as out:
        out.setparams((1, 2, RATE, 0, "NONE", "not compressed"))
        out.writeframes(data)


def edit_distance(left, right):
    row = list(range(len(right) + 1))
    for i, a in enumerate(left, 1):
        nxt = [i]
        for j, b in enumerate(right, 1):
            nxt.append(min(row[j] + 1, nxt[-1] + 1, row[j-1] + (a != b)))
        row = nxt
    return row[-1]


def transcribe(binary, python, slot, audio):
    cmd = [binary, "transcribe", python, CACHE, slot, audio]
    with subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          env={"PATH": "/usr/bin:/bin"}) as proc:
        start = time.monotonic()
        observed = 0
        try:
            while proc.poll() is None:
                if time.monotonic() - start > 180:
                    raise TimeoutError("Timed out processing the qualification fixture")
                marker = slot / "last-observed.json"
                if marker.is_file():
                    child_pid = int(json.loads(marker.read_text())["pid"])
                    stats = subprocess.run(["/bin/ps", "-o", "pid=,ppid=,rss=", "-p",
                                            f"{proc.pid},{child_pid}"], capture_output=True, text=True)
                    if stats.returncode not in (0, 1) or stats.stderr.strip():
                        raise RuntimeError("Could not observe RSS of the owned speech processes")
                    rows = [tuple(map(int, s.split())) for s in stats.stdout.splitlines()]
                    children = [rss for pid, ppid, rss in rows if pid == child_pid and ppid == proc.pid]
                    host = [rss for pid, _, rss in rows if pid == proc.pid]
                    if host and children:
                        observed = max(observed, 1024 * (host[0] + children[0]))
                time.sleep(0.2)
        finally:
            if proc.poll() is None:
                proc.send_signal(signal.SIGINT)
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
        out, err = proc.communicate()
        exits=[json.loads(f.read_bytes()) for f in slot.glob("rss-*.json")]
        require(len(exits)==2 and len({x["attempt"] for x in exits})==2 and
                all(x["parent"]==proc.pid and x["combined_rss_bytes"]>0 and x["backend_peak_rss_bytes"]>0 for x in exits),
                "Could not observe both real preflight and inference exits for "+audio.name+": "+err.decode()[:180])
        observed=max(observed,*(x["combined_rss_bytes"] for x in exits))
        return proc.returncode, out, err.decode(), {
            "elapsed_seconds": round(time.monotonic() - start, 3),
            "max_observed_host_plus_child_rss_bytes": observed,
            "max_backend_peak_rss_bytes": max(x["backend_peak_rss_bytes"] for x in exits),
            "measured_backend_exits": len(exits),
        }


def main():
    require(sys.platform == "darwin", "This RSS qualifier uses Darwin ps")
    rustup = shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo = Path(subprocess.check_output([rustup, "which", "--toolchain", "1.97.1", "cargo"], text=True).strip())
    subprocess.run([cargo, "build", "--locked", "--offline", "-p", "orange-model-runtime", "--bin", "orange-transcribe"],
                   cwd=ROOT, env=dict(os.environ, PATH=str(cargo.parent) + os.pathsep + os.environ["PATH"]), check=True)
    binary = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")).resolve() / "debug/orange-transcribe"
    results = {}
    with tempfile.TemporaryDirectory(prefix="orange-asr-corpus-") as folder:
        folder = Path(folder)
        # exec preserves the pid and standard streams. The observer executes the
        # exact native host's runner, registering only an at-exit resource reading.
        python=folder/"observe-python"
        interpreter=shlex.quote(str(ROOT/".local/transcription-venv/bin/python"))
        observer=shlex.quote(str(ROOT/"scripts/transcription_rss_observer.py"))
        python.write_text(f'#!/bin/sh\nexec {interpreter} -I -B {observer} "$@"\n')
        python.chmod(0o700)
        for language in ("zh", "en", "silence"):
            audio = folder / (language + ".wav")
            if language == "silence":
                write_wav(audio, bytes(2 * FRAMES))
            else:
                repeats = make_fixture(folder, language)
            digest = hashlib.sha256(audio.read_bytes()).hexdigest()
            observations = []
            for run in range(2):
                slot = folder / f"{language}-{run}"
                slot.mkdir(mode=0o700)
                code, out, err, metrics = transcribe(binary, python, slot, audio)
                if language == "silence":
                    require(code != 0 and not out and "NoSpeech" in err, "Silence returned a transcript")
                else:
                    require(code == 0, "Speech transcription failed: " + err[:200])
                    receipt = json.loads(out)
                    measured, t = receipt["measure"], receipt["transcript"]
                    require(receipt["audio_sha256"] == digest and measured["source_samples"] == FRAMES
                            and measured["normalized_samples"] == FRAMES and measured["duration_ms"] == 60000,
                            "Unexpected corpus measurement")
                    require(t["language"] == language and t["segments"], "Language detection or segments failed")
                    ref = CORPUS[language]["reference"]
                    if language == "zh":
                        reference = list(ref * repeats)
                        actual = re.findall(r"[\u3400-\u9fff]", t["text"])
                    else:
                        reference = ref.split() * repeats
                        actual = re.findall(r"[a-z]+", t["text"].lower())
                    errors = edit_distance(reference, actual)
                    require(errors * 10 <= len(reference), "Reference error rate exceeds 10%")
                    metrics.update(reference_units=len(reference), edit_distance=errors,
                                   segment_count=len(t["segments"]), detected_language=t["language"],
                                   transcript_sha256=hashlib.sha256(out).hexdigest())
                observations.append(metrics)
            if language != "silence":
                require(observations[0]["transcript_sha256"] == observations[1]["transcript_sha256"],
                        "Repeat produced different segments, language, measurement or text")
            results[language] = {"fixture_sha256": digest, "runs": observations}
    dest = ROOT / ".local/transcription-evidence"
    dest.mkdir(parents=True, exist_ok=True)
    report = {"model": MODEL["model_version"], "duration_ms_each": 60000,
              "fixture_method": "Complete repetitions of pinned human recordings with one-second gaps, then digital silence to 60 seconds",
              "memory_method": "Approximate 200 ms parent+child RSS samples plus a simultaneous exit sample for each real backend invocation. Separate Darwin RUSAGE_SELF high-water bytes per backend. RSS sums can count shared memory twice.",
              "results": results}
    (dest / "corpus.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))


if __name__ == "__main__":
    main()
