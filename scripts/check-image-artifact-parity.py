#!/usr/bin/env python3.12
"""Exercise the native inspector and the installed CUDA supervisor's PNG routine on identical bytes."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import struct
import subprocess
import zlib

from PIL import Image
from sdxl_png import inspect_png


def chunk(kind, payload):
    return struct.pack(">I",len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload))


def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--inspector",required=True,type=Path)
    cli.add_argument("--actual-png",required=True,type=Path)
    cli.add_argument("--expected-sha256",required=True)
    args=cli.parse_args()
    data=args.actual_png.read_bytes()
    if hashlib.sha256(data).hexdigest()!=args.expected_sha256:
        raise RuntimeError("Different real model output")
    def verify(case,payload,allowed):
        try:
            measured=inspect_png(payload)
        except (ValueError,OSError):
            measured=None
        result=subprocess.run([args.inspector.resolve(),"--inspect-artifact"],input=payload,capture_output=True,timeout=20)
        if not allowed:
            if measured is not None or result.returncode==0 or result.stdout:
                raise RuntimeError("Invalid output accepted: "+case)
        elif measured is None or result.returncode!=0 or json.loads(result.stdout)!=measured:
            raise RuntimeError("Inspectors disagree on "+case)
        print(case,"accepted" if allowed else "rejected",flush=True)
        return measured
    facts=verify("actual_model_png",data,True)
    # Split IDAT at a new boundary. Content identity changes, decoded pixels must agree.
    size=int.from_bytes(data[33:37],"big");assert data[37:41]==b"IDAT" and size>=2
    body=data[41:41+size]
    repacked=data[:33]+chunk(b"IDAT",body[:size//2])+chunk(b"IDAT",body[size//2:])+data[45+size:]
    amended=verify("rechunked_same_pixels",repacked,True)
    assert amended["rgb_sha256"]==facts["rgb_sha256"] and amended["sha256"]!=facts["sha256"]
    invalid={
        "progress_json":b'{"phase":"denoising","completed_steps":1}',
        "truncated":data[:-1], "trailing":data+b"x", "bad_terminal_crc":data[:-1]+bytes([data[-1]^1]),
        "extra_png":data+data, "missing_idat":data[:33]+data[-12:],
        "unexpected_chunk":data[:33]+chunk(b"tEXt",b"marker")+data[33:],
        "late_header":data[:33]+data[8:33]+data[33:],
        "wrong_dimensions":data[:8]+chunk(b"IHDR",struct.pack(">I",2048)+data[20:29])+data[33:],
        "oversized":b"x"*((16<<20)+1),
    }
    buf=io.BytesIO()
    Image.new("RGB",(1024,1024),(22,33,44)).save(buf,format="PNG")
    invalid["constant_rgb"]=buf.getvalue()
    for case,payload in invalid.items():verify(case,payload,False)
    print(json.dumps({"native_and_python_verified":True,"actual":facts,"positive":2,"rejected":len(invalid),"ledger_operations":0}),flush=True)


if __name__=="__main__":
    main()
