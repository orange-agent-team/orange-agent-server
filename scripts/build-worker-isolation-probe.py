#!/usr/bin/env python3
"""Build the tiny, dependency-free Linux diagnostic payload with a fixed Rust compiler."""
import argparse
from pathlib import Path
import shutil
import struct
import subprocess

ROOT=Path(__file__).resolve().parents[1]
VERSION="1.97.1"
TARGETS={"x86_64-unknown-linux-musl":62,"aarch64-unknown-linux-musl":183}


def build(source,dest,target):
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    for toolchain in (VERSION,"stable"):
        rustc=Path(subprocess.check_output([rustup,"which","--toolchain",toolchain,"rustc"],text=True).strip())
        version=subprocess.check_output([rustc,"--version"],text=True).strip()
        if not version.startswith("rustc "+VERSION+" "):continue
        lib=Path(subprocess.check_output([rustc,"--target",target,"--print","target-libdir"],text=True).strip())
        if lib.is_dir():break
    else:raise ValueError("Missing exact Rust host or Linux standard library")
    sysroot=Path(subprocess.check_output([rustc,"--print","sysroot"],text=True).strip())
    linker,=sysroot.glob("lib/rustlib/*/bin/rust-lld")
    subprocess.run([rustc,"--edition","2024","--crate-name","orange_isolation_probe","--target",target,
                    "--remap-path-prefix",str(source.parent)+"=/orange/worker-isolation","-C","opt-level=s","-C","panic=abort",
                    "-C","strip=symbols","-C",f"linker={linker}","-C","linker-flavor=ld.lld",source,"-o",dest],check=True)
    verify_image(dest,target)


def verify_image(dest,target):
    image=dest.read_bytes();header=image[:20]
    if header[:6]!=b"\x7fELF\x02\x01" or int.from_bytes(header[18:20],"little")!=TARGETS[target]:
        raise ValueError("Wrong probe architecture")
    phoff=struct.unpack_from("<Q",image,32)[0];stride,count=struct.unpack_from("<HH",image,54)
    if stride<56 or not 0<count<512 or phoff+stride*count>len(image):
        raise ValueError("Invalid probe program headers")
    for index in range(count):
        entry=phoff+stride*index
        kind=struct.unpack_from("<I",image,entry)[0]
        if kind==3:raise ValueError("Probe requires an external interpreter")
        if kind==2:
            begin=struct.unpack_from("<Q",image,entry+8)[0];size=struct.unpack_from("<Q",image,entry+32)[0]
            if size%16 or begin+size>len(image) or any(tag==1 for tag,_ in struct.iter_unpack("<qQ",image[begin:begin+size])):
                raise ValueError("Probe has an external dynamic dependency")


if __name__=="__main__":
    p=argparse.ArgumentParser();p.add_argument("--target",choices=TARGETS,required=True);p.add_argument("--output",type=Path,required=True)
    args=p.parse_args()
    build(ROOT/"distribution/windows-worker/isolation/probe.rs",args.output.resolve(),args.target)
