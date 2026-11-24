#!/usr/bin/env python3.12
"""Measure the fixed SDXL template on an explicitly selected local GPU; preserve incomplete runs."""
import argparse
import csv
import re
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import time
import uuid

from sdxl_device_recovery import store_journal

ROOT = Path(__file__).resolve().parent
PROFILE = "sdxl-base-1.0-euler-fp16-cuda-v1"
PROMPT = "A single orange on a white plate."
LIMIT = 65536


def require(ok, reason):
    if not ok:
        raise RuntimeError(reason)


def execute(command, request, cancel_at_step):
    started = time.monotonic()
    completed = 0
    cancelled = False
    emitted = bytearray()
    fragment = b""
    error_bytes = 0
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
        try:
            process.stdin.write(json.dumps(request).encode())
            process.stdin.close()
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                selector.register(process.stderr, selectors.EVENT_READ)
                while selector.get_map():
                    require(time.monotonic()-started < 2000, "Benchmark deadline exceeded")
                    for key, _ in selector.select(timeout=1):
                        part = os.read(key.fileobj.fileno(), 8192)
                        if not part:
                            selector.unregister(key.fileobj)
                        elif key.fileobj is process.stdout:
                            emitted.extend(part)
                            require(len(emitted) <= LIMIT, "Receipt exceeds limit")
                        else:
                            error_bytes += len(part)
                            require(error_bytes <= LIMIT, "Diagnostics exceed limit")
                            lines = (fragment + part).split(b"\n")
                            fragment = lines.pop()
                            for line in lines:
                                if not line.startswith(b"{"):
                                    continue
                                event = json.loads(line)
                                require(type(event.get("completed_steps")) is int and event == {
                                    "phase":"denoising", "attempt_id":request["attempt_id"], "completed_steps":completed+1
                                } and completed < 25, "Unexpected benchmark progress")
                                completed += 1
                                if cancel_at_step and not cancelled:
                                    process.send_signal(signal.SIGTERM)
                                    cancelled = True
            status = process.wait(timeout=20)
        finally:
            if process.poll() is None:
                process.terminate()
                try: process.wait(timeout=120)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)
    return status, bytes(emitted), completed, cancelled, time.monotonic()-started


def validate_receipt(value, request, output):
    from PIL import Image
    require(value["attempt_id"] == request["attempt_id"] and value["model_version"] == PROFILE and value["seed"] == request["seed"]
            and value["gpu_release_observed"] is True and value["steps"] == 25 and value["media_type"] == "image/png"
            and (value["width"],value["height"]) == (1024,1024), "Wrong output receipt")
    require(type(value["peak_allocated_bytes"]) is int and type(value["peak_reserved_bytes"]) is int
            and 0 < value["peak_allocated_bytes"] <= value["peak_reserved_bytes"], "Missing GPU memory measurements")
    with output.open("rb") as stream:
        require(os.fstat(stream.fileno()).st_size == value["bytes"] and hashlib.file_digest(stream,"sha256").hexdigest() == value["sha256"],
                "Published PNG changed")
        stream.seek(0)
        with Image.open(stream) as decoded:
            require(decoded.format == "PNG" and decoded.mode == "RGB" and decoded.size == (1024,1024)
                    and hashlib.sha256(decoded.tobytes()).hexdigest() == value["rgb_sha256"], "Published pixels changed")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--qualification", required=True, type=Path)
    parser.add_argument("--qualification-sha256", required=True)
    parser.add_argument("--gpu-uuid",required=True)
    parser.add_argument("--weights",type=Path,required=True)
    parser.add_argument("--work-root",type=Path,required=True)
    parser.add_argument("--reports",type=Path,required=True,help="Existing private directory outside weights and work-root")
    parser.add_argument("--context",default="default")
    args = parser.parse_args()
    require(sys.platform=="linux" and os.getuid()!=0,"Physical benchmark requires the non-root owner on the target Linux host")
    from importlib.util import module_from_spec,spec_from_file_location
    spec=spec_from_file_location("orange_target",ROOT/"run-linux-sdxl-attempt.py")
    supervisor=module_from_spec(spec)
    spec.loader.exec_module(supervisor)
    require(re.fullmatch(r"GPU-[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}",args.gpu_uuid),"One specific GPU required")
    work=supervisor.local_dir(args.work_root)
    weights=supervisor.local_dir(args.weights)
    reports=supervisor.local_dir(args.reports)
    require(not any(a.is_relative_to(b) for a,b in [(reports,weights),(weights,reports),(reports,work),(work,reports)]),
            "Report directory overlaps model or device state")
    require(len(args.qualification_sha256)==64 and hashlib.sha256(args.qualification.read_bytes()).hexdigest()==args.qualification_sha256,
            "Different operator-approved qualification")
    import tempfile
    suite=Path(tempfile.mkdtemp(prefix="sdxl-benchmark-",dir=reports))
    report={"model":PROFILE,"gpu":args.gpu_uuid,"qualification_sha256":args.qualification_sha256,"cases":[],"status":"running",
            "public_worker_admission":False}
    def save():
        store_journal(suite,report)
    save()
    try:
        smi=next(path for path in (Path("/usr/bin/nvidia-smi"),Path("/usr/lib/wsl/lib/nvidia-smi")) if path.is_file())
        fields=next(csv.reader(subprocess.check_output([smi,"-i",args.gpu_uuid,"--query-gpu=uuid,name,driver_version,memory.total",
                              "--format=csv,noheader,nounits"],text=True,timeout=8).splitlines(),skipinitialspace=True))
        require(len(fields)==4 and fields[0].lower()==args.gpu_uuid.lower() and fields[3].isdigit(),"Incomplete physical device inventory")
        report["driver"]={"name":fields[1],"driver_version":fields[2],"total_memory_mib":int(fields[3])}
        save()
        for kind,seed in [("first",7341),("repeat",7341),("invalid-seed",-1),("cancel-after-step",7342),("after-cancel",7341)]:
            request={"protocol":1,"attempt_id":str(uuid.uuid4()),"model_version":PROFILE,"prompt":PROMPT,"seed":seed}
            output=suite/(kind+".png")
            cmd=[sys.executable,ROOT/"run-linux-sdxl-attempt.py","--qualification",args.qualification,"--qualification-sha256",args.qualification_sha256,
                 "--gpu-uuid",args.gpu_uuid,"--weights",weights,"--work-root",work,"--output",output,"--context",args.context,"--observe-progress"]
            baseline=supervisor.gpu_usage(args.gpu_uuid)
            status,raw,steps,cancelled,elapsed=execute(cmd,request,kind=="cancel-after-step")
            require(not (work/("gpu-"+args.gpu_uuid.lower())/"attempt.json").exists(), "Target device still fenced")
            require(supervisor.recovered_gpu(args.gpu_uuid,baseline),"Missing independent final driver observations")
            row={"case":kind,"attempt_id":request["attempt_id"],"elapsed_seconds":elapsed,"observed_steps":steps,
                 "status":status,"baseline_used_mib":baseline,"observed_quiescence":True}
            if kind in ("cancel-after-step","invalid-seed"):
                require(status!=0 and not raw and not output.exists(), "Failed request published an artifact")
                if kind=="cancel-after-step":require(cancelled and 0<steps<25,"Cancellation did not follow an actual step")
                else:require(steps==0 and not cancelled,"Rejected request executed")
            else:
                require(status==0 and not cancelled,"Physical generation failed")
                receipt=json.loads(raw)
                validate_receipt(receipt,request,output)
                require(receipt["device_uuid"]==args.gpu_uuid,"Different physical device")
                row["receipt"]=receipt
            report["cases"].append(row)
            save()
        report["repeat_pixels_equal"]=len({case["receipt"]["rgb_sha256"] for case in report["cases"] if "receipt" in case})==1
        report["status"]="passed_local_development_benchmark"
        save()
        print(suite/"attempt.json",flush=True)
    except BaseException as error:
        report.update(status="incomplete",failure_type=type(error).__name__)
        save()
        raise


if __name__=="__main__":
    main()
