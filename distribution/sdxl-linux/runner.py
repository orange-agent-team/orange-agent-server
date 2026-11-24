"""Fixed offline Linux CUDA SDXL adapter. One private input, one attempt and one validated candidate PNG."""
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import signal
import sys
import tempfile
import uuid

APP=Path(__file__).resolve().parent
PROFILE="sdxl-base-1.0-euler-fp16-cuda-v1"
SITE=Path("/opt/orange/lib/python3.12/site-packages")
MAX_PNG=16*1024*1024


class Rejected(Exception):pass
class DeviceUnavailable(Exception):pass


def unique(pairs):
    result={}
    for k,v in pairs:
        if k in result:raise Rejected("duplicate field")
        result[k]=v
    return result


def frame(raw):
    if len(raw)>8192:raise Rejected("frame exceeds 8192 bytes")
    value=json.loads(raw.decode("utf-8"),object_pairs_hook=unique)
    if not isinstance(value,dict) or set(value)!={"protocol","model_version","attempt_id","prompt","seed"}:raise Rejected("invalid fields")
    if type(value["protocol"]) is not int or value["protocol"]!=1 or value["model_version"]!=PROFILE:raise Rejected("unrecognized model/version")
    if not isinstance(value["attempt_id"],str) or str(uuid.UUID(value["attempt_id"]))!=value["attempt_id"]:raise Rejected("invalid attempt")
    if type(value["seed"]) is not int or not 0<=value["seed"]<=4294967295:raise Rejected("invalid seed")
    prompt=value["prompt"]
    if not isinstance(prompt,str) or not prompt.strip() or "\x00" in prompt or len(prompt.encode())>2048:raise Rejected("invalid prompt")
    return value


def emit(value):
    print(json.dumps(value,separators=(",",":"),allow_nan=False),flush=True)


def profile():
    base=json.loads((APP/"base-image.lock.json").read_bytes())
    weights=json.loads((APP/"model.lock.json").read_bytes())
    template={**weights["template"],"device":"cuda"}
    return {"model_version":PROFILE,"platform":"linux/amd64","base_manifest":base["manifest_digest"],
            "wheel_lock_sha256":hashlib.sha256((APP/"wheels.lock.json").read_bytes()).hexdigest(),
            "weight_lock_sha256":hashlib.sha256((APP/"model.lock.json").read_bytes()).hexdigest(),
            "template":template,"admission":"requires_independent_hardware_and_license_review"}


def environment():
    from importlib.metadata import version
    if sys.platform!="linux" or platform.machine()!="x86_64" or sys.version_info[:3]!=(3,12,13):raise RuntimeError("incorrect target")
    sys.path.insert(0,str(SITE))
    wheels=json.loads((APP/"wheels.lock.json").read_bytes())["wheels"]
    if any(version(x["package"])!=x["version"] for x in wheels):raise RuntimeError("environment drift")
    # Isolated Python ignores ambient PYTHONPATH. Add only this image's fixed dependency and helper directories.
    sys.path.insert(0,str(APP))
    import torch
    from diffusers import EulerDiscreteScheduler,StableDiffusionXLPipeline
    from transformers import CLIPTokenizer
    from imwatermark import WatermarkEncoder
    if not torch.version.cuda or not all((EulerDiscreteScheduler,StableDiffusionXLPipeline,CLIPTokenizer,WatermarkEncoder)):
        raise RuntimeError("CUDA SDXL dependencies unavailable")
    return torch


def device(torch):
    if not torch.cuda.is_available() or torch.cuda.device_count()!=1:raise DeviceUnavailable()
    major,minor=torch.cuda.get_device_capability(0)
    properties=torch.cuda.get_device_properties(0)
    return {"device":0,"architecture":f"sm_{major}{minor}","memory_bytes":properties.total_memory,"torch_cuda":torch.version.cuda}


def generate(request,torch):
    from diffusers import EulerDiscreteScheduler,StableDiffusionXLPipeline
    from diffusers.utils import logging as dl
    from transformers import CLIPTokenizer
    from transformers.utils import logging as tl
    from PIL import Image
    import model_assets
    import warnings
    dl.disable_progress_bar();tl.disable_progress_bar()
    cfg=json.loads((APP/"model.lock.json").read_bytes())
    weights=Path("/weights");work=Path("/scratch");output=Path("/result")
    if not os.statvfs(weights).f_flag & os.ST_RDONLY:raise Rejected("read-only weight mount required")
    for folder in (work,output):model_assets.private_dir(folder)
    if any(output.iterdir()):raise Rejected("empty private output directory required")
    with tempfile.TemporaryDirectory(dir=work,prefix="verified-weights-") as stage:
        tree=Path(stage);files=cfg["files"]
        tokens={key:v for key,v in files.items() if key.split("/")[0] in ("tokenizer","tokenizer_2")}
        model_assets.snapshot(weights,tokens,tree)
        for name in ("tokenizer","tokenizer_2"):
            tok=CLIPTokenizer.from_pretrained(tree/name,local_files_only=True)
            ids=tok(request["prompt"],truncation=False,add_special_tokens=True,verbose=False).input_ids
            if not ids or len(ids)>77 or tok.model_max_length!=77:raise Rejected("prompt token limit")
        model_assets.snapshot(weights,{k:v for k,v in files.items() if k not in tokens},tree)
        torch.set_num_threads(2)
        torch.cuda.reset_peak_memory_stats(0)
        pipe=StableDiffusionXLPipeline.from_pretrained(tree,local_files_only=True,use_safetensors=True,variant="fp16",torch_dtype=torch.float16,
                                                      add_watermarker=True)
        if type(pipe) is not StableDiffusionXLPipeline or type(pipe.scheduler) is not EulerDiscreteScheduler or not pipe.vae.config.force_upcast or pipe.watermark is None:
            raise RuntimeError("unreviewed pipeline")
        pipe.enable_attention_slicing("auto");pipe.enable_vae_tiling();pipe.set_progress_bar_config(disable=True);pipe.to("cuda:0")
        steps=[]
        def progress(_,index,timestep,values):
            if index!=len(steps) or index>=25:raise RuntimeError("invalid progress")
            torch.cuda.synchronize(0);steps.append(index)
            emit({"phase":"denoising","attempt_id":request["attempt_id"],"completed_steps":len(steps)})
            return values
        with torch.inference_mode(),warnings.catch_warnings():
            warnings.filterwarnings("error",category=RuntimeWarning)
            result=pipe(prompt=request["prompt"],width=1024,height=1024,num_inference_steps=25,num_images_per_prompt=1,guidance_scale=5.0,
                        generator=torch.Generator(device="cpu").manual_seed(request["seed"]),callback_on_step_end=progress,
                        callback_on_step_end_tensor_inputs=[],output_type="pil")
        torch.cuda.synchronize(0)
        if len(steps)!=25 or len(result.images)!=1:raise RuntimeError("incomplete inference")
        image=result.images[0]
        if image.size!=(1024,1024) or image.mode!="RGB" or all(lo==hi for lo,hi in image.getextrema()):raise RuntimeError("invalid result")
        raw=io.BytesIO();image.save(raw,format="PNG");data=raw.getvalue()
        if not 0<len(data)<=MAX_PNG:raise RuntimeError("invalid PNG size")
        with Image.open(io.BytesIO(data)) as decoded:
            decoded.load()
            if decoded.format!="PNG" or decoded.size!=image.size or decoded.mode!="RGB" or decoded.tobytes()!=image.tobytes():raise RuntimeError("PNG integrity error")
        # A result remains a candidate until the supervising host observes successful exit and verifies its receipt.
        with (output/"candidate.png").open("xb") as out:
            out.write(data);out.flush();os.fsync(out.fileno())
        return {"phase":"complete","model_version":PROFILE,"attempt_id":request["attempt_id"],"seed":request["seed"],"width":1024,"height":1024,
                "steps":len(steps),"media_type":"image/png","bytes":len(data),"sha256":hashlib.sha256(data).hexdigest(),
                "rgb_sha256":hashlib.sha256(image.tobytes()).hexdigest(),"peak_allocated_bytes":torch.cuda.max_memory_allocated(0),
                "peak_reserved_bytes":torch.cuda.max_memory_reserved(0)}


def main():
    if sys.argv[1:]==["describe"]:emit(profile());return 0
    if sys.argv[1:] not in (["probe"],["generate"]):raise Rejected("unknown mode")
    request=None
    if sys.argv[1]=="generate":
        try:request=frame(sys.stdin.buffer.read(8193))
        except (ValueError,TypeError,UnicodeError) as exc:raise Rejected("invalid frame") from exc
    signal.alarm(1800)
    torch=environment()
    hardware=device(torch)
    if request is None:emit({"state":"observed","model_version":PROFILE,**hardware});return 0
    emit(generate(request,torch));return 0


if __name__=="__main__":
    try:code=main()
    except Rejected:emit({"error":"invalid_image_request"});code=2
    except DeviceUnavailable:emit({"error":"device_unavailable"});code=3
    except Exception as exc:
        # No request content or environment values in diagnostics.
        print("SDXL runner failure",type(exc).__name__,file=sys.stderr,flush=True)
        emit({"error":"image_backend_unavailable"});code=4
    sys.exit(code)
