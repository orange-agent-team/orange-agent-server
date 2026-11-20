"""One supervised, bounded MC-05 CPU operation over a private binary stdin frame."""

import hashlib
import io
import os
import stat
import fcntl
import signal
import json
import math
from pathlib import Path
import struct
import sys
import tempfile
import uuid

MAX_BYTES = 100_000_000
MAX_SECONDS = 600
RATE = 16_000
MODEL = "whisper-small-ct2-int8-cpu-v1"
ASSET_SET_SHA256 = "02dcb025e83fd4cdc11f2a375dd67fbaefbd8774822661696a923302c99bb222"
FILES = {"config.json", "model.bin", "tokenizer.json", "vocabulary.txt"}


class InputError(Exception):
    pass


class NoSpeech(Exception):
    pass


class MediaLimit(Exception):
    pass


def wav_samples(data):
    if (
        len(data) < 12
        or data[8:12] != b"WAVE"
        or int.from_bytes(data[4:8], "little") + 8 != len(data)
    ):
        raise InputError("invalid wav size")
    offset = 12
    fmt = None
    payload = None
    while offset < len(data):
        if offset + 8 > len(data):
            raise InputError("short chunk header")
        tag = data[offset : offset + 4]
        size = int.from_bytes(data[offset + 4 : offset + 8], "little")
        start = offset + 8
        end = start + size
        if end > len(data):
            raise InputError("truncated chunk")
        if tag == b"fmt ":
            if fmt is not None or size < 16:
                raise InputError("invalid fmt")
            fmt = struct.unpack_from("<HHIIHH", data, start)
        if tag == b"data":
            if payload is not None:
                raise InputError("ambiguous data")
            payload = size
        offset = end + (size % 2)
    if offset != len(data) or fmt is None or not payload:
        raise InputError("missing PCM data")
    codec, channels, hz, byte_rate, align, bits = fmt
    if (
        codec not in (1, 3, 65534)
        or not 1 <= channels <= 8
        or not 8000 <= hz <= 192000
        or align != channels * ((bits + 7) // 8)
        or not align
        or byte_rate != hz * align
        or payload % align
    ):
        raise InputError("invalid PCM layout")
    samples = payload // align
    if samples > hz * MAX_SECONDS:
        raise MediaLimit
    return samples, hz


def format_name(data):
    if data.startswith(b"RIFF") and data[8:12] == b"WAVE":
        return "wav"
    if data.startswith(b"fLaC"):
        return "flac"
    if data.startswith(b"OggS"):
        return "ogg"
    if data.startswith(b"ID3") or (
        len(data) >= 2 and data[0] == 255 and data[1] & 0xE0 == 0xE0
    ):
        return "mp3"
    if len(data) >= 12 and data[4:8] == b"ftyp":
        return "mov"
    raise InputError("unsupported format")


def decode(data):
    import av
    import numpy as np

    demux = format_name(data)
    pcm_spec = wav_samples(data) if demux == "wav" else None
    options = {
        "err_detect": "crccheck+explode",
        "protocol_whitelist": "pipe",
        "format_whitelist": demux,
    }
    if demux == "mov":
        options.update(enable_drefs="0", use_absolute_path="0")
    pcm = bytearray()

    def append(frames):
        for frame in frames:
            if len(pcm) + frame.samples * 2 > RATE * MAX_SECONDS * 2:
                raise MediaLimit
            raw = frame.to_ndarray().astype("<i2", copy=False).tobytes()
            if len(raw) != frame.samples * 2:
                raise InputError("resampler shape")
            pcm.extend(raw)

    samples = 0
    try:
        with av.open(
            io.BytesIO(data), mode="r", format=demux, options=options
        ) as container:
            if len(container.streams) != 1 or len(container.streams.audio) != 1:
                raise InputError("one audio stream required")
            stream = container.streams.audio[0]
            allowed = {
                "wav": {
                    "pcm_s16le",
                    "pcm_s24le",
                    "pcm_s32le",
                    "pcm_u8",
                    "pcm_f32le",
                    "pcm_f64le",
                },
                "flac": {"flac"},
                "ogg": {"opus", "vorbis", "flac"},
                "mp3": {"mp3", "mp3float"},
                "mov": {"aac", "alac"},
            }
            codec = stream.codec_context.name
            if codec not in allowed[demux]:
                raise InputError("unsupported audio codec")
            hz = stream.codec_context.sample_rate
            if not hz or not 8000 <= hz <= 192000:
                raise InputError("sample rate")
            resampler = av.AudioResampler(format="s16", layout="mono", rate=RATE)
            for packet in container.demux(stream):
                if packet.is_corrupt:
                    raise InputError("corrupt packet")
                for frame in packet.decode():
                    if (
                        frame.is_corrupt
                        or frame.sample_rate != hz
                        or not 1 <= len(frame.layout.channels) <= 8
                    ):
                        raise InputError("unexpected frame")
                    samples += frame.samples
                    if samples > hz * MAX_SECONDS:
                        raise MediaLimit
                    # Meter decoded samples; timestamps in the container cannot inject silence or reduce usage.
                    frame.pts = None
                    append(resampler.resample(frame))
            append(resampler.resample(None))
    except av.error.FFmpegError as exc:
        raise InputError("decode failed") from exc
    if not samples or not pcm or (pcm_spec and pcm_spec != (samples, hz)):
        raise InputError("incomplete audio")
    if demux == "flac":
        if (
            len(data) < 42
            or data[4] & 127 != 0
            or int.from_bytes(data[5:8], "big") != 34
        ):
            raise InputError("FLAC STREAMINFO")
        declared = int.from_bytes(data[18:26], "big") & ((1 << 36) - 1)
        if declared and declared != samples:
            raise InputError("incomplete FLAC samples")
    measured = {
        "preprocessing": "mono-16khz-s16-f32-v1",
        "format": demux,
        "codec": codec,
        "sample_rate": hz,
        "source_samples": samples,
        "normalized_samples": len(pcm) // 2,
        "normalized_sha256": hashlib.sha256(pcm).hexdigest(),
        "duration_ms": (samples * 1000 + hz - 1) // hz,
    }
    return measured, np.frombuffer(pcm, dtype="<i2").astype(np.float32) / 32768.0


def phase(attempt, state):
    directory = Path(sys.argv[2])
    with tempfile.NamedTemporaryFile(dir=directory, mode="w", delete=False) as tmp:
        name = Path(tmp.name)
        try:
            json.dump(
                {
                    "protocol": 1,
                    "attempt_id": attempt,
                    "phase": state,
                    "pid": os.getpid(),
                },
                tmp,
            )
            tmp.flush()
            os.replace(name, directory / "last-observed.json")
        finally:
            name.unlink(missing_ok=True)


def infer(root, pin, audio, measured, language, attempt):
    from faster_whisper import WhisperModel
    from orange_inference_backends.embedding import verified_bytes

    if (
        set(pin) != FILES
        or hashlib.sha256(
            json.dumps(pin, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()
        != ASSET_SET_SHA256
    ):
        raise ValueError("fixed assets required")
    assets = {name: verified_bytes(root, name, pin[name]) for name in sorted(FILES)}
    with tempfile.TemporaryDirectory(prefix="orange-asr-") as private:
        model = WhisperModel(
            private,
            files=assets,
            device="cpu",
            compute_type="int8",
            cpu_threads=2,
            num_workers=1,
            local_files_only=True,
        )
        if model.model.device != "cpu" or model.model.compute_type not in (
            "int8",
            "int8_float32",
        ):
            raise ValueError("unqualified compute profile")
        compute_type = model.model.compute_type
        if language is not None and language not in model.supported_languages:
            raise InputError("language")
        phase(attempt, "inference")
        segments, info = model.transcribe(
            audio,
            task="transcribe",
            language=language,
            temperature=0.0,
            beam_size=1,
            best_of=1,
            vad_filter=False,
            word_timestamps=False,
            condition_on_previous_text=True,
        )
        actual = []
        previous = 0
        text_size = 0
        for item in segments:
            if not math.isfinite(item.start) or not math.isfinite(item.end):
                raise ValueError("invalid time")
            start, end = round(1000 * item.start), round(1000 * item.end)
            if start < previous or end < start or end > measured["duration_ms"]:
                raise ValueError("invalid segment boundary")
            text_size += len(item.text.encode("utf-8"))
            if len(actual) >= 10000 or text_size > 1024 * 1024:
                raise MediaLimit
            actual.append({"start_ms": start, "end_ms": end, "text": item.text})
            previous = end
        if (
            info.language not in model.supported_languages
            or not 0 <= info.language_probability <= 1
        ):
            raise ValueError("language result")
    return {
        "compute_type": compute_type,
        "language": info.language,
        "language_probability": info.language_probability,
        "segments": actual,
        "text": "".join(s["text"] for s in actual).strip(),
    }


def execute():
    stream = sys.stdin.buffer
    length = stream.read(4)
    if len(length) != 4:
        raise InputError("frame")
    n = int.from_bytes(length, "big")
    if not 1 <= n <= 16384:
        raise InputError("frame")
    frame = json.loads(stream.read(n))
    if (
        set(frame)
        != {
            "protocol",
            "action",
            "model_version",
            "audio_bytes",
            "audio_sha256",
            "language",
            "files",
            "expected_measure",
            "attempt_id",
        }
        or frame["protocol"] != 1
        or frame["action"] not in ("measure", "transcribe")
        or frame["model_version"] != MODEL
    ):
        raise InputError("invalid frame")
    attempt = frame["attempt_id"]
    if (
        not isinstance(attempt, str)
        or len(attempt) != 36
        or str(uuid.UUID(attempt)) != attempt
    ):
        raise InputError("attempt")
    count = frame["audio_bytes"]
    if type(count) is not int or count < 1:
        raise InputError("size")
    if count > MAX_BYTES:
        raise MediaLimit
    language = frame["language"]
    if language is not None and (
        not isinstance(language, str)
        or not language.isascii()
        or not language.isalpha()
        or not language.islower()
        or not 2 <= len(language) <= 3
    ):
        raise InputError("language")
    data = stream.read(count)
    sha = hashlib.sha256(data).hexdigest()
    if len(data) != count or stream.read(1) or sha != frame["audio_sha256"]:
        raise InputError("input digest")
    phase(attempt, "decoding")
    measured, audio = decode(data)
    result = {"model_version": MODEL, "audio_sha256": sha, "measure": measured}
    if frame["action"] == "transcribe":
        if measured != frame["expected_measure"]:
            return {"error": "measurement_changed"}
        if not audio.any():
            raise NoSpeech
        result["transcript"] = infer(
            Path(sys.argv[1]), frame["files"], audio, measured, language, attempt
        )
        if not result["transcript"]["text"]:
            raise NoSpeech
    phase(attempt, "candidate_ready")
    return result


def claim_slot(path):
    folder = path.lstat()
    if (
        not stat.S_ISDIR(folder.st_mode)
        or folder.st_uid != os.geteuid()
        or folder.st_mode & 0o077
    ):
        raise ValueError("private runtime directory required")
    fd = os.open(
        path / "transcription.lock",
        os.O_CREAT | os.O_RDWR | os.O_NONBLOCK | os.O_NOFOLLOW,
        0o600,
    )
    info = os.fstat(fd)
    if (
        not stat.S_ISREG(info.st_mode)
        or info.st_uid != os.geteuid()
        or info.st_nlink != 1
        or info.st_mode & 0o077
    ):
        raise ValueError("invalid slot file")
    # Retain this file description until process exit, even if the launching host is lost.
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    return fd


if __name__ == "__main__":
    try:
        signal.signal(signal.SIGALRM, signal.SIG_DFL)
        signal.alarm(660)
        slot = claim_slot(Path(sys.argv[2]))
        ready = {"ready": True, "protocol": 1}
        if "ORANGE_ASR_INSTANCE" in os.environ:
            instance = str(uuid.UUID(os.environ["ORANGE_ASR_INSTANCE"]))
            info = os.fstat(slot)
            with Path(sys.executable).resolve(strict=True).open("rb") as executable:
                digest = hashlib.file_digest(executable, "sha256").hexdigest()
            ready.update(instance=instance, pid=os.getpid(), python_sha256=digest,
                         slot_dev=str(info.st_dev), slot_ino=str(info.st_ino))
        print(json.dumps(ready, separators=(",", ":")), flush=True)
        result = execute()
    except BlockingIOError:
        result = {"error": "slot_busy"}
    except NoSpeech:
        result = {"error": "no_speech"}
    except MediaLimit:
        result = {"error": "limit"}
    except (InputError, UnicodeError, json.JSONDecodeError):
        result = {"error": "invalid_audio_request"}
    except Exception:
        result = {"error": "transcription_backend_unavailable"}
    print(json.dumps(result, separators=(",", ":"), allow_nan=False), flush=True)
