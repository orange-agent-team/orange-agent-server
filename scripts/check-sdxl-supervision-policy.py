#!/usr/bin/env python3.12
"""Validate typed admission and GPU evidence handling without claiming a physical GPU on the builder."""
from importlib.util import module_from_spec,spec_from_file_location
import json
from pathlib import Path
import uuid

ROOT=Path(__file__).resolve().parents[1]
def load(name,path):
    spec=spec_from_file_location(name,path);module=module_from_spec(spec);spec.loader.exec_module(module);return module

r=load("sdxl_input",ROOT/"distribution/sdxl-linux/runner.py")
h=load("sdxl_owner",ROOT/"scripts/run-linux-sdxl-attempt.py")
frame={"protocol":1,"model_version":r.PROFILE,"attempt_id":str(uuid.uuid4()),"prompt":"橙","seed":4294967295}
assert r.frame(json.dumps(frame).encode())==frame
for raw in [json.dumps(dict(frame,seed=True)),json.dumps(dict(frame,output="/tmp/other")),json.dumps(dict(frame,prompt="橙"*683)),
            json.dumps(frame).replace('"seed":','"seed":2,"seed":')]:
    try:r.frame(raw.encode())
    except (r.Rejected,ValueError):pass
    else:raise ValueError("Unsafe frame admitted")
def series(values):
    items=iter(values)
    def sample(device):
        v=next(items,None)
        if v is None:raise RuntimeError("unknown")
        return v
    return sample
assert h.recovered_gpu("fixture",20,series([70,20,None,20,19]),.001,.1)
assert not h.recovered_gpu("fixture",20,series([21,20,21]),.001,.02)
assert not h.recovered_gpu("fixture",20,series([]),.001,.02)
print("PASS bound input, duplicate rejection, repeated clean device observations and refusal of unknown or elevated memory; driver samples are explicit fixtures")
