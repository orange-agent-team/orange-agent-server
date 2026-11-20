"""Shared host-side fixed toolchain and archive checks for the development OCI backend."""
import hashlib
from pathlib import Path
import shutil
import subprocess

def digest(path):
    h=hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024*1024), b''):
            h.update(block)
    return h.hexdigest()

def build_payload(policy,source,probe):
    rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
    rustc=Path(subprocess.check_output([rustup,'which','--toolchain','stable','rustc'],text=True).strip())
    version=subprocess.check_output([rustc,'--version'],text=True).strip()
    if not version.startswith('rustc '+policy['rust_version']+' '):
        raise SystemExit('The fixed Rust stable compiler is required')
    sysroot=Path(subprocess.check_output([rustc,'--print','sysroot'],text=True).strip())
    linkers=list(sysroot.glob('lib/rustlib/*/bin/rust-lld'))
    if len(linkers)!=1:
        raise SystemExit('A single host rust-lld from the pinned compiler is required')
    subprocess.run([rustc,'--edition','2024','--target',policy['target'],'-C','opt-level=s','-C','strip=symbols',
                    '-C',f'linker={linkers[0]}','-C','linker-flavor=ld.lld',source,'-o',probe],check=True)
    header=probe.read_bytes()[:20]
    if header[:6]!=b'\x7fELF\x02\x01' or int.from_bytes(header[18:20],'little')!=183:
        raise SystemExit('The managed payload must be a Linux ARM64 ELF')
    return version
