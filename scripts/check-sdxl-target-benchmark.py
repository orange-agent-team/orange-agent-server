#!/usr/bin/env python3.12
"""Test the progress/cancellation protocol and kernel mount parsing without claiming a GPU observation."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile

from sdxl_progress import StepObserver

ROOT=Path(__file__).resolve().parents[1]
def module(name,path):
    spec=importlib.util.spec_from_file_location(name,path)
    value=importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def main():
    runner=module("sdxl_runner",ROOT/"distribution/sdxl-linux/runner.py")
    benchmark=module("sdxl_benchmark",ROOT/"scripts/benchmark-sdxl-device.py")
    def mount(where,flags):
        return f"401 7 0:1 / {where} {flags},relatime - ext4 /dev/fixture rw\n"
    fixed=mount("/weights","ro")
    assert runner.readonly_weights(fixed)
    assert runner.readonly_weights(fixed+mount("/weights/tokenizer","ro"))
    for records in ("",mount("/elsewhere","ro"),mount("/weights","rw"),fixed+mount("/weights/unet","rw"),fixed+fixed):
        assert not runner.readonly_weights(records)
    try:runner.readonly_weights("unknown")
    except runner.Rejected:pass
    else:raise AssertionError("Unreadable mount info accepted")
    def step(n):
        return {"phase":"denoising","completed_steps":n,"attempt_id":"sample"}
    with tempfile.TemporaryFile() as stream:
        observer=StepObserver("sample")
        wire=json.dumps(step(1)).encode()+b"\n"
        stream.write(wire[:5]);stream.flush()
        assert not observer.read(stream.fileno()) and stream.tell()==5
        stream.write(wire[5:]);stream.flush()
        assert observer.read(stream.fileno())==[step(1)] and stream.tell()==len(wire)
        assert not observer.read(stream.fileno())
        for n in range(2,26):stream.write(json.dumps(step(n)).encode()+b"\n")
        stream.write(b'{"phase":"complete"}\n');stream.flush()
        assert len(observer.read(stream.fileno()))==24
    for invalid in [step(2),dict(step(1),completed_steps=True),dict(step(1),prompt="private"),{"phase":"complete"}]:
        with tempfile.TemporaryFile() as stream:
            stream.write(json.dumps(invalid).encode()+b"\n");stream.flush()
            try:StepObserver("sample").read(stream.fileno())
            except ValueError:pass
            else:raise AssertionError("Invalid progress accepted")
    request={"attempt_id":"sample"}
    cmd=[sys.executable,"-c","import sys,time;sys.stdin.read();print("+repr(json.dumps(step(1)))+",file=sys.stderr,flush=True);time.sleep(30)"]
    status,raw,steps,terminated,_=benchmark.execute(cmd,request,True)
    assert status!=0 and not raw and steps==1 and terminated
    import hashlib
    from PIL import Image
    with tempfile.TemporaryDirectory() as tmp:
        png=Path(tmp)/"result.png"
        image=Image.new("RGB",(1024,1024),(235,116,40))
        image.save(png)
        request={"attempt_id":"fixture","seed":6}
        receipt={**request,"model_version":benchmark.PROFILE,"steps":25,"gpu_release_observed":True,"media_type":"image/png",
                 "width":1024,"height":1024,"bytes":png.stat().st_size,"sha256":hashlib.sha256(png.read_bytes()).hexdigest(),
                 "rgb_sha256":hashlib.sha256(image.tobytes()).hexdigest(),"peak_allocated_bytes":1,"peak_reserved_bytes":2}
        benchmark.validate_receipt(receipt,request,png)
        for field,value in (("rgb_sha256","0"*64),("peak_allocated_bytes",0),("steps",24),("width",1000),("media_type","image/jpeg")):
            try:benchmark.validate_receipt({**receipt,field:value},request,png)
            except RuntimeError:pass
            else:raise AssertionError("Unqualified PNG or sample accepted")
        png.write_bytes(png.read_bytes()[:-7])
        try:benchmark.validate_receipt(receipt,request,png)
        except RuntimeError:pass
        else:raise AssertionError("Damaged output accepted")
    print("PASS mount namespace policies, partial progress reads, fixed step identity and real process cancellation after a fixture step; no GPU benchmark claimed")


if __name__=="__main__":
    main()
