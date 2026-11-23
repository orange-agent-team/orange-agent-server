#!/usr/bin/env python3
"""Cross-build and inspect two independent Windows x64 GPU inventory artifacts on macOS."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import struct
import subprocess
import tarfile
import tempfile
import zipfile
from rust_notices import notices

ROOT=Path(__file__).resolve().parents[1]
TARGET="x86_64-pc-windows-gnullvm"


def digest(file):
    with file.open("rb") as f:
        return hashlib.file_digest(f,"sha256").hexdigest()


def inspect(exe, llvm, *, bundled=(), dll=False):
    data=exe.read_bytes()
    if data[:2]!=b"MZ" or len(data)<64:raise ValueError("Missing DOS header")
    offset=struct.unpack_from("<I",data,60)[0]
    if data[offset:offset+4]!=b"PE\0\0" or struct.unpack_from("<H",data,offset+4)[0]!=0x8664:
        raise ValueError("Expected x64 Windows PE image")
    flags=struct.unpack_from("<H",data,offset+22)[0]
    if not flags & 2 or bool(flags & 0x2000) != dll:
        raise ValueError("Incorrect executable or DLL image characteristics")
    if struct.unpack_from("<H",data,offset+24)[0]!=0x20b:raise ValueError("Expected PE32+ image")
    details=subprocess.check_output([llvm,"--coff-imports",exe],text=True)
    dlls=sorted(set(re.findall(r"(?im)^\s*Name:\s*([^\s]+\.dll)\s*$",details)))
    system={"advapi32","bcrypt","bcryptprimitives","crypt32","kernel32","kernelbase","ntdll","ole32",
            "oleaut32","user32","userenv","ws2_32","msvcrt","ucrtbase","shell32","iphlpapi","secur32",
            "cfgmgr32","dbghelp","gdi32","mswsock","ncrypt","psapi","schannel","synchronization"}
    if not dlls or any(not d.lower().startswith("api-ms-win-") and d.lower()[:-4] not in system and d.lower() not in bundled for d in dlls):
        raise ValueError("Inspect deployment dependencies before packaging: "+str(dlls))
    return dlls


def main():
    if platform.system()!="Darwin":raise ValueError("Pinned macOS cross-toolchain required")
    parser=argparse.ArgumentParser()
    parser.add_argument("--vendor", type=Path,help="Existing verified versioned cargo vendor tree")
    opts=parser.parse_args()
    commit=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    def source(name):
        return subprocess.check_output(["git","show",commit+":"+name],cwd=ROOT)
    lock=json.loads(source("config/windows-cross-toolchain.lock.json"))
    if lock["target"]!=TARGET:raise ValueError("Unexpected cross target")
    archive=ROOT/".local/windows-cross"/lock["archive"]
    if digest(archive)!=lock["sha256"] or archive.stat().st_size!=lock["bytes"]:
        raise ValueError("Prepare the fixed toolchain first")
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain",lock["rust"],"cargo"],text=True).strip())
    installed=subprocess.check_output([rustup,"target","list","--installed","--toolchain",lock["rust"]],text=True).splitlines()
    if TARGET not in installed:raise ValueError("Missing pinned Windows standard library")
    dest=ROOT/".local/windows-worker-distributions"
    dest.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(dir=dest,prefix=".qualify-") as directory:
        scratch=Path(directory)
        with tarfile.open(archive,"r:xz") as ar:ar.extractall(scratch,filter="data")
        tools=scratch/lock["archive"].removesuffix(".tar.xz")/"bin"
        driver=tools/"x86_64-w64-mingw32-clang"
        epoch=subprocess.check_output(["git","show","-s","--format=%ct",commit],cwd=ROOT,text=True).strip()
        licenses={"licenses/llvm/LLVM-LICENSE.TXT":(tools.parent/"LICENSE.TXT").read_bytes()}
        mingw=tools.parent/"x86_64-w64-mingw32/share/mingw32"
        for path in sorted(mingw.glob("COPYING*")):
            licenses["licenses/mingw/"+path.name]=path.read_bytes()
        if "licenses/mingw/COPYING.MinGW-w64-runtime.txt" not in licenses:raise ValueError("Missing runtime notice")
        rust_docs=cargo.parent.parent/"share/doc/rust"
        licenses["licenses/rust/COPYRIGHT-library.html"]=(rust_docs/"COPYRIGHT-library.html").read_bytes()
        for path in sorted((rust_docs/"licenses").glob("*")):
            if path.is_file():licenses["licenses/rust/"+path.name]=path.read_bytes()
        results=[]
        for n in (1,2):
            tree=scratch/str(n)
            tree.mkdir()
            with tempfile.TemporaryFile() as stream:
                subprocess.run(["git","archive",commit],cwd=ROOT,stdout=stream,check=True)
                stream.seek(0)
                with tarfile.open(fileobj=stream) as ar:ar.extractall(tree,filter="data")
            if n==1:
                vendor=opts.vendor.resolve() if opts.vendor else scratch/"vendor"
                if opts.vendor is None:
                    subprocess.run([cargo,"vendor","--locked","--offline","--versioned-dirs",vendor],cwd=tree,
                                   env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"]),check=True,stdout=subprocess.DEVNULL)
                _,missing=notices(licenses,vendor,source("Cargo.lock"),source)
                if missing:raise ValueError("Missing Cargo license notices")
            prefix="CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_"
            suffix="x86_64_pc_windows_gnullvm"
            env=dict(os.environ,CARGO_NET_OFFLINE="true",CARGO_INCREMENTAL="0",CARGO_TARGET_DIR=str(tree/"target"),
                     CARGO_PROFILE_RELEASE_STRIP="symbols",SOURCE_DATE_EPOCH=epoch,ZERO_AR_DATE="1",
                     PATH=os.pathsep.join([str(cargo.parent),str(tools),os.environ["PATH"]]))
            env.setdefault("CARGO_BUILD_JOBS","2")
            env[prefix+"LINKER"]=str(driver)
            env[prefix+"RUSTFLAGS"]=f"--remap-path-prefix={tree}=/orange-server -C link-arg=-Wl,--no-insert-timestamp"
            env["CC_"+suffix]=str(driver)
            env["CXX_"+suffix]=str(tools/"x86_64-w64-mingw32-clang++")
            env["AR_"+suffix]=str(tools/"x86_64-w64-mingw32-ar")
            env["CFLAGS_"+suffix]=f"-ffile-prefix-map={tree}=/orange-server"
            subprocess.run([cargo,"build","--locked","--offline","--target",TARGET,"--release","-p","orange-worker",
                            "--bin","orange-worker"],cwd=tree,env=env,check=True)
            exe=tree/"target"/TARGET/"release/orange-worker.exe"
            imports=inspect(exe,tools/"llvm-readobj",bundled={"libunwind.dll"})
            runtime={}
            if any(i.lower()=="libunwind.dll" for i in imports):
                lib=tools.parent/"x86_64-w64-mingw32/bin/libunwind.dll"
                runtime[lib.name]=inspect(lib,tools/"llvm-readobj",dll=True)
            system_imports=sorted({i.lower() for i in imports if i.lower() not in runtime}|
                                  {i.lower() for module in runtime.values() for i in module})
            payload={"bin/orange-worker.exe":exe.read_bytes(),"README.md":source("distribution/windows-worker/README.md"),
                     "docs/nvidia-inventory.md":source("docs/worker/nvidia-inventory.md"),
                     "Test-WorkerInventory.ps1":source("distribution/windows-worker/Test-WorkerInventory.ps1"),
                     "Cargo.lock":source("Cargo.lock"),"rust-toolchain.toml":source("rust-toolchain.toml"),
                     "windows-cross-toolchain.lock.json":source("config/windows-cross-toolchain.lock.json")}
            for name in ("README.md","wsl.conf","oci-base.json","prepare-inference-policy.py","inspect-target.py"):
                payload["isolation/"+name]=source("distribution/windows-worker/isolation/"+name)
            payload.update(licenses)
            for name in runtime:
                payload["bin/"+name]=(tools.parent/"x86_64-w64-mingw32/bin"/name).read_bytes()
            manifest={"source":commit,"target":TARGET,"rust":lock["rust"],"toolchain_sha256":lock["sha256"],
                      "imported_system_dlls":system_imports,"bundled_runtime_imports":runtime,"windows_runtime_verified":False,
                      "sha256":{k:hashlib.sha256(v).hexdigest() for k,v in payload.items()}}
            payload["manifest.json"]=(json.dumps(manifest,sort_keys=True,indent=2)+"\n").encode()
            output=scratch/f"worker-{n}.zip"
            with zipfile.ZipFile(output,"x",compression=zipfile.ZIP_DEFLATED,compresslevel=9) as bundle:
                for name,value in sorted(payload.items()):
                    item=zipfile.ZipInfo("orange-worker/"+name,(1980,1,1,0,0,0))
                    item.create_system=3
                    item.external_attr=(0o100755 if name.endswith(".exe") else 0o100644)<<16
                    bundle.writestr(item,value,compress_type=zipfile.ZIP_DEFLATED,compresslevel=9)
            results.append((output,manifest))
        checksum=digest(results[0][0])
        if digest(results[1][0])!=checksum or results[0][1]!=results[1][1]:
            raise ValueError("Independent Windows builds differ")
        final=dest/f"orange-worker-preflight-win-x64-{checksum[:16]}.zip"
        if final.exists():
            if digest(final)!=checksum:raise ValueError("Archive identity collision")
        else:os.link(results[0][0],final)
        with zipfile.ZipFile(final) as verified:
            if verified.testzip() is not None:raise ValueError("Damaged ZIP")
            for name,expected in results[0][1]["sha256"].items():
                if hashlib.sha256(verified.read("orange-worker/"+name)).hexdigest()!=expected:raise ValueError("Member digest changed")
        report={"archive":final.name,"sha256":checksum,"bytes":final.stat().st_size,"source":commit,"target":TARGET,
                "identical_archives":2,"imported_system_dlls":results[0][1]["imported_system_dlls"],
                "bundled_runtime":sorted(results[0][1]["bundled_runtime_imports"]),
                "windows_runtime_verified":False,"physical_gpu_verified":False}
        final.with_suffix(".json").write_text(json.dumps(report,indent=2)+"\n")
        print(json.dumps(report),flush=True)


if __name__=="__main__":
    main()
