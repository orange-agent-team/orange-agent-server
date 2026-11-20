"""Fixed, offline SDXL operation. A private caller owns staging, deadlines and final publication."""
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import signal
import shutil
import stat
import sys
import tempfile
import uuid
import warnings

PROFILE = "sdxl-base-1.0-euler-fp16-mps-v1"
PROFILE_SHA = "958fd0c50403021cf5eca04fe663070ea61c4fbbcdd7686552ffbe4a21e0c288"
MAX_PNG = 16 * 1024 * 1024


class InvalidRequest(Exception):
    pass


class Unsupported(Exception):
    pass


def unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise InvalidRequest("duplicate key")
        result[key] = value
    return result


def request(stream):
    data = stream.read(8193)
    if not data or len(data) > 8192:
        raise InvalidRequest("invalid request size")
    try:
        value = json.loads(data.decode("utf-8"), object_pairs_hook=unique)
        if set(value) != {"protocol", "attempt_id", "model_version", "prompt", "seed"}:
            raise InvalidRequest("request fields")
        if type(value["protocol"]) is not int or value["protocol"] != 1 or value["model_version"] != PROFILE:
            raise InvalidRequest("protocol")
        if str(uuid.UUID(value["attempt_id"])) != value["attempt_id"]:
            raise InvalidRequest("attempt")
        text = value["prompt"]
        if not isinstance(text, str) or not text.strip() or len(text.encode()) > 2048 or "\x00" in text:
            raise InvalidRequest("invalid prompt")
        if type(value["seed"]) is not int or not 0 <= value["seed"] <= 4294967295:
            raise InvalidRequest("invalid seed")
    except (TypeError, ValueError, KeyError, UnicodeError, AttributeError) as exc:
        raise InvalidRequest("invalid frame") from exc
    return value


def private_dir(path):
    info = path.lstat()
    if not path.is_absolute() or not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("private owner-controlled directory required")


def snapshot(cache, files, dest):
    private_dir(cache)
    for name, spec in files.items():
        relative = Path(name)
        if relative.is_absolute() or ".." in relative.parts or len(relative.parts) > 2:
            raise ValueError("invalid approved name")
        source = cache / relative
        private_dir(source.parent)
        with os.fdopen(os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), "rb") as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_size != spec["bytes"] or info.st_mode & 0o022:
                raise ValueError("unexpected model file")
            target = dest / relative
            target.parent.mkdir(mode=0o700, exist_ok=True)
            h = hashlib.sha256()
            with target.open("xb") as out:
                remaining = spec["bytes"]
                while remaining:
                    block = stream.read(min(4*1024*1024,remaining))
                    if not block:
                        raise ValueError("incomplete asset")
                    h.update(block)
                    out.write(block)
                    remaining -= len(block)
                if stream.read(1) or h.hexdigest() != spec["sha256"]:
                    raise ValueError("model asset changed")
    # Only these copied and verified bytes are offered to Diffusers/Transformers.


def emit(message):
    print(json.dumps(message, separators=(",", ":"), allow_nan=False), flush=True)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def run(frame, pin, cache, attempt_dir):
    from importlib.metadata import version
    import platform
    if sys.platform != "darwin" or platform.machine() != "arm64" or sys.version_info[:2] != (3,12):
        raise Unsupported("unsupported runtime target")
    if any(version(name) != expected for name, expected in pin["environment"]["packages"].items()):
        raise Unsupported("unapproved runtime environment")
    import torch
    from diffusers import StableDiffusionXLPipeline, EulerDiscreteScheduler
    from PIL import Image
    from transformers import CLIPTokenizer
    if not torch.backends.mps.is_available():
        raise Unsupported("mps")
    torch.set_num_threads(2)
    with tempfile.TemporaryDirectory(dir=attempt_dir, prefix="weights-") as tree:
        emit({"phase":"verifying", "attempt_id":frame["attempt_id"]})
        dest = Path(tree)
        files = pin["files"]
        token_files = {name: spec for name, spec in files.items() if name.split("/")[0] in ("tokenizer", "tokenizer_2")}
        snapshot(cache, token_files, dest)
        encoded = []
        for name in ("tokenizer","tokenizer_2"):
            tokenizer = CLIPTokenizer.from_pretrained(dest/name, local_files_only=True)
            ids = tokenizer(frame["prompt"], truncation=False, add_special_tokens=True, verbose=False).input_ids
            if not ids or len(ids) > 77 or tokenizer.model_max_length != 77:
                raise InvalidRequest("prompt exceeds a fixed encoder window")
            encoded.append(ids)
        snapshot(cache, {name:spec for name,spec in files.items() if name not in token_files}, dest)
        emit({"phase":"loading", "attempt_id":frame["attempt_id"]})
        pipe = StableDiffusionXLPipeline.from_pretrained(tree, variant="fp16", torch_dtype=torch.float16,
            local_files_only=True, use_safetensors=True, add_watermarker=True)
        if type(pipe.scheduler) is not EulerDiscreteScheduler or pipe.watermark is None or not pipe.vae.config.force_upcast:
            raise ValueError("unapproved pipeline configuration")
        for tokenizer, ids in zip((pipe.tokenizer,pipe.tokenizer_2), encoded, strict=True):
            if tokenizer(frame["prompt"],truncation=False,verbose=False).input_ids != ids:
                raise ValueError("pipeline tokenization differs from preflight")
        pipe.set_progress_bar_config(disable=True)
        pipe.enable_attention_slicing()
        pipe.enable_vae_tiling()
        pipe.to("mps")
        device = torch.device("mps")
        if any(next(part.parameters()).device.type != device.type for part in (pipe.unet,pipe.vae,pipe.text_encoder,pipe.text_encoder_2)):
            raise Unsupported("model device")
        start_steps = []
        peak_tensors = torch.mps.current_allocated_memory()
        peak_driver = torch.mps.driver_allocated_memory()
        def step_end(_, index, timestep, values):
            nonlocal peak_tensors, peak_driver
            torch.mps.synchronize()
            peak_tensors = max(peak_tensors, torch.mps.current_allocated_memory())
            peak_driver = max(peak_driver, torch.mps.driver_allocated_memory())
            if index != len(start_steps):
                raise ValueError("unexpected diffusion step")
            start_steps.append(index)
            emit({"phase":"denoising","completed_steps":index+1,"attempt_id":frame["attempt_id"]})
            return values
        with torch.inference_mode(), warnings.catch_warnings():
            warnings.filterwarnings("error", category=RuntimeWarning)
            output = pipe(prompt=frame["prompt"], width=1024, height=1024, num_inference_steps=25,
                num_images_per_prompt=1, guidance_scale=5.0, generator=torch.Generator(device="cpu").manual_seed(frame["seed"]),
                callback_on_step_end=step_end,callback_on_step_end_tensor_inputs=[],output_type="pil")
        torch.mps.synchronize()
        if len(start_steps) != 25 or len(output.images) != 1:
            raise ValueError("incomplete output")
        image = output.images[0]
        if image.mode != "RGB" or image.size != (1024,1024) or all(lo==hi for lo,hi in image.getextrema()):
            raise ValueError("invalid decoded image")
        png = io.BytesIO()
        image.save(png,format="PNG")
        data=png.getvalue()
        if not 0 < len(data) <= MAX_PNG:
            raise ValueError("image size")
        with Image.open(io.BytesIO(data)) as check:
            check.verify()
        with (attempt_dir/"candidate.png").open("xb") as file:
            file.write(data);file.flush();os.fsync(file.fileno())
        return {"phase":"complete","attempt_id":frame["attempt_id"],"model_version":PROFILE,
                "seed":frame["seed"],"width":1024,"height":1024,"steps":len(start_steps),"media_type":"image/png",
                "bytes":len(data),"sha256":digest(data),"rgb_sha256":digest(image.tobytes()),
                "observed_tensor_bytes":max(peak_tensors,torch.mps.current_allocated_memory()),
                "observed_driver_bytes":max(peak_driver,torch.mps.driver_allocated_memory())}


def main():
    if len(sys.argv)!=5:
        raise InvalidRequest("internal image host arguments")
    os.umask(0o077)
    for key in ("HF_HUB_OFFLINE","TRANSFORMERS_OFFLINE","DIFFUSERS_OFFLINE","HF_HUB_DISABLE_TELEMETRY"):
        os.environ[key]="1"
    signal.alarm(1800)
    config, cache, slot, attempt_dir = map(Path,sys.argv[1:])
    frame=request(sys.stdin.buffer)
    raw=config.read_bytes()
    if digest(raw)!=PROFILE_SHA:
        raise ValueError("profile digest")
    private_dir(slot);private_dir(attempt_dir)
    if (attempt_dir.parent != slot or not attempt_dir.name.startswith("image-attempt-" + frame["attempt_id"] + "-")
        or config != attempt_dir / "profile.json"):
        raise InvalidRequest("unrecognized private attempt")
    identity = attempt_dir.stat()
    parent = int(os.environ["ORANGE_IMAGE_PARENT_PID"])
    lock=os.open(slot/"sdxl.lock",os.O_CREAT|os.O_RDWR|os.O_NOFOLLOW|os.O_NONBLOCK,0o600)
    try:
        info=os.fstat(lock)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink!=1 or info.st_uid!=os.getuid() or info.st_mode & 0o077:
            raise ValueError("private lock required")
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        emit(run(frame,json.loads(raw),cache,attempt_dir))
    finally:
        os.close(lock)
        # After host loss, the next write to its progress pipe fails. Only reclaim
        # the nonce-specific attempt we observed, after dropping the device lock.
        if os.getppid() != parent and attempt_dir.exists():
            current = attempt_dir.lstat()
            if stat.S_ISDIR(current.st_mode) and (current.st_dev,current.st_ino) == (identity.st_dev,identity.st_ino):
                shutil.rmtree(attempt_dir)


if __name__ == "__main__":
    try:
        main()
    except InvalidRequest:
        emit({"error":"invalid_image_request"});sys.exit(2)
    except Unsupported:
        emit({"error":"device_unavailable"});sys.exit(3)
    except BlockingIOError:
        emit({"error":"image_slot_busy"});sys.exit(4)
    except Exception as exc:
        # Diagnostics go only to the owning supervisor. Never include prompts in error frames.
        print(type(exc).__name__, file=sys.stderr, flush=True)
        emit({"error":"image_backend_unavailable"});sys.exit(5)
