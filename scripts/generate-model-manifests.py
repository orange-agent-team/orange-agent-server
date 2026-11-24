#!/usr/bin/env python3.12
"""Compile the four bundled reference manifests from versioned asset locks; no network or mutable cache input."""
import argparse
import hashlib
import json
from pathlib import Path

ROOT=Path(__file__).resolve().parents[1]
DEST=ROOT/"crates/model-runtime/model-manifests"


def sha(path):
    return hashlib.sha256((ROOT/path).read_bytes()).hexdigest()


def compile_manifest(kind):
    lock_path=f"config/local-{kind}.lock.json"
    source=json.loads((ROOT/lock_path).read_bytes())
    assets=source.get("files",{})
    if kind=="model":
        version="qwen-local-test-v1"
        model=source["model"]
        assets={model["file"]:model}
        engine={"engine":"llama_cpp","source_revision":source["backend"]["commit"],"build_recipe_sha256":sha("scripts/prepare-local-model.py")}
        io={"modality":"text","input":"chat_messages","output":"generated_text",
            "context_tokens":json.loads((ROOT/"config/worker-development.example.json").read_bytes())["profile"]["context_tokens"]}
    else:
        version=source["model_version"]
        model=source
        engine={"engine":{"embedding":"onnx_runtime","image":"diffusers","transcription":"faster_whisper"}[kind],
                "dependency_lock_sha256":sha("backends/environments/sdxl-macos/uv.lock" if kind=="image" else "backends/uv.lock"),
                "adapter_sha256":sha(f"backends/orange_inference_backends/{kind}.py")}
        if kind=="embedding":
            io={"modality":"embedding","input":"text_batch","output":"float_vectors","max_batch":source["max_batch"],
                "max_tokens_per_text":source["max_tokens_per_text"],"dimensions":source["dimensions"]}
        elif kind=="image":
            template=source["template"]
            if template["device"]!="mps" or template["images"]!=1 or engine["dependency_lock_sha256"]!=source["environment"]["lock_sha256"]:
                raise ValueError("Image template or Python environment differs from the reviewed lock")
            io={"modality":"image","input":"text_prompt","output":"png","width":template["width"],"height":template["height"],
                "steps":template["steps"],"max_prompt_utf8_bytes":template["max_prompt_utf8_bytes"]}
        else:
            io={"modality":"transcription","input":"audio_file","output":"transcript","channels":source["channels"],"sample_rate":source["sample_rate"],
                "max_input_bytes":source["max_input_bytes"],"max_decoded_ms":source["max_decoded_ms"]}
    files=[]
    for path,entry in sorted(assets.items()):
        local=entry["sha256"]+".gguf" if kind=="model" else (Path(path).name if kind=="embedding" else path)
        files.append({"source_path":path,"cache_path":local,"bytes":entry["bytes"],"sha256":entry["sha256"]})
    return {"schema_version":1,"model_version":version,"source":{"repository":model["repository"],"revision":model["revision"],"license":model["license"],
            "asset_lock_sha256":sha(lock_path)},"artifacts":files,"runtime":engine,"interface":io,
            "resources":{"device":"metal" if kind=="image" else "cpu","artifact_bytes":sum(f["bytes"] for f in files)},
            "pricing":{"policy":"local_unpriced"}}


def cuda_draft():
    # Weights and content specification are identical to the reviewed SDXL lock. The
    # deployment target, image identity and model version are separate from the MPS model.
    candidate=compile_manifest("image")
    profile=json.loads((ROOT/"distribution/sdxl-linux/qualification.json").read_bytes())
    if (profile["platform"]!="linux/amd64" or not profile["no_device_denied"] or
        profile["wheel_lock_sha256"]!=sha("backends/environments/sdxl-linux-cuda/wheels.lock.json") or
        profile["system_lock_sha256"]!=sha("backends/environments/sdxl-linux-cuda/debian-libs.lock.json") or
        len(profile["runner_installations"])!=2 or not all(r["runner_sha256"]==sha("distribution/sdxl-linux/runner.py") for r in profile["runner_installations"])):
        raise ValueError("Qualified runtime differs from tracked runner or dependency locks")
    candidate["model_version"]="sdxl-base-1.0-euler-fp16-cuda-v1"
    candidate["runtime"]={"engine":"diffusers","image_digest":profile["image"]}
    candidate["resources"]["device"]="nvidia_gpu"
    return candidate


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--write",action="store_true",help="Update the checked-in manifest inputs; default is to verify")
    args=parser.parse_args()
    expected={}
    for kind in ("model","embedding","image","transcription"):
        doc=compile_manifest(kind);name=doc["model_version"]+".json"
        expected[name]=(json.dumps(doc,indent=2,ensure_ascii=False)+"\n").encode()
    if args.write:
        DEST.mkdir(parents=True,exist_ok=True)
        for name,payload in expected.items():(DEST/name).write_bytes(payload)
    if set(expected)!={f.name for f in DEST.iterdir()} or any((DEST/k).is_symlink() or (DEST/k).read_bytes()!=v for k,v in expected.items()):
        raise ValueError("Shipped model descriptors differ from reviewed local source and engine locks")
    path=ROOT/"distribution/sdxl-linux/model-version.json"
    payload=(json.dumps(cuda_draft(),indent=2,ensure_ascii=False)+"\n").encode()
    if args.write:path.write_bytes(payload)
    if path.is_symlink() or path.read_bytes()!=payload:raise ValueError("CUDA draft differs from qualified OCI and weight inputs")
    print("PASS unpriced CUDA model draft binds locked assets and immutable OCI manifest")
    print("PASS four bundled ModelVersion descriptors match immutable asset, engine and adapter inputs")


if __name__=="__main__":main()
