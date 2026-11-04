#!/usr/bin/env python3
"""Build pinned CPU-only llama.cpp and verify the official free test-model artifact."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import shutil
import tarfile

ROOT = Path(__file__).resolve().parents[1]
PRIVATE = ROOT/'.local/model-runtime'
LOCK = json.loads((ROOT/'config/local-model.lock.json').read_text())

def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024*1024), b''):
            h.update(chunk)
    return h.hexdigest()

def download(source):
    rustup = shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
    cargo = Path(subprocess.check_output([rustup, 'which', '--toolchain', 'stable', 'cargo'], text=True).strip())
    environment = os.environ.copy()
    environment['PATH'] = str(cargo.parent)+os.pathsep+environment.get('PATH', '')
    subprocess.run([str(cargo), 'build', '--bin', 'model-fetch', '--locked'], cwd=ROOT, env=environment, check=True)
    subprocess.run([str(ROOT/'target/debug/model-fetch'), str(ROOT/'config/local-model.lock.json'), str(PRIVATE), '--source='+source],
                   cwd=ROOT, env=environment, check=True)

def build(checkout, target):
    if checkout is None:
        raise SystemExit('--source-checkout is mandatory for explicit fixed source import')
    commit = LOCK['backend']['commit']
    subprocess.run(['git', '-C', str(checkout), 'cat-file', '-e', commit+'^{commit}'], check=True)
    source = PRIVATE/('llama-'+commit)
    if not source.exists():
        archive = PRIVATE/'source.tar'
        with archive.open('wb') as file:
            subprocess.run(['git', '-C', str(checkout), 'archive', commit], stdout=file, check=True)
        staging = PRIVATE/'source-staging'
        staging.mkdir(exist_ok=True)
        with tarfile.open(archive) as tar:
            # Export only regular files/directories, no executable extraction hooks or escaping links.
            for member in tar.getmembers():
                path = Path(member.name)
                if path.is_absolute() or '..' in path.parts or not (member.isfile() or member.isdir()):
                    raise SystemExit('Source archive contains unsupported path/type; review before import')
            tar.extractall(staging)
        os.replace(staging, source)
        archive.unlink()
    tree = subprocess.check_output(['git', '-C', str(checkout), 'ls-tree', '-rz', commit])
    source_hash, expected = hashlib.sha256(), set()
    for entry in tree.split(b'\0'):
        if not entry: continue
        metadata, raw_name = entry.split(b'\t', 1)
        mode, kind, object_id = metadata.split()
        name = raw_name.decode('utf-8')
        if mode not in [b'100644', b'100755'] or kind != b'blob':
            raise SystemExit('Unsupported fixed Git tree entry')
        file = source/name
        if file.is_symlink() or not file.is_file():
            raise SystemExit('Missing or linked source in fixed dependency cache')
        data = file.read_bytes()
        git_digest = hashlib.sha1(b'blob '+str(len(data)).encode()+b'\0'+data).hexdigest()
        if git_digest != object_id.decode():
            raise SystemExit('Cached dependency differs from pinned Git object; not compiling')
        expected.add(name)
        source_hash.update(mode+b' '+raw_name+b'\0'+hashlib.sha256(data).digest())
    actual = {str(p.relative_to(source)) for p in source.rglob('*') if p.is_file() or p.is_symlink()}
    if actual != expected:
        raise SystemExit('Unexpected files in pinned dependency source cache')
    output = PRIVATE/'build'
    environment = os.environ.copy()
    # An exported archive has no Git metadata. Never let upstream discover Orange's parent Git.
    environment['GIT_CEILING_DIRECTORIES'] = str(PRIVATE)
    subprocess.run(['cmake', '-S', str(source), '-B', str(output), '-DCMAKE_BUILD_TYPE=Release',
                    '-DBUILD_SHARED_LIBS=OFF', '-DGGML_METAL=OFF', '-DGGML_CUDA=OFF',
                    '-DGGML_VULKAN=OFF', '-DGGML_RPC=OFF', '-DGGML_NATIVE=OFF',
                    '-DLLAMA_BUILD_TESTS=OFF', '-DLLAMA_BUILD_EXAMPLES=OFF', '-DLLAMA_BUILD_APP=OFF',
                    '-DLLAMA_BUILD_SERVER=ON', '-DLLAMA_BUILD_UI=OFF', '-DLLAMA_USE_PREBUILT_UI=OFF',
                    '-DLLAMA_CONNECT=OFF', '-DLLAMA_SUBPROCESS=OFF', '-DLLAMA_OPENSSL=OFF'], env=environment, check=True)
    subprocess.run(['cmake', '--build', str(output), '--target', target, '-j', '2'], env=environment, check=True)
    binary = output/'bin'/target
    report = {'backend_commit': commit, 'source_tree_sha256': source_hash.hexdigest(), 'executable_sha256': digest(binary),
              'model_sha256': LOCK['model']['sha256'], 'gpu_enabled': False}
    report['target'] = target
    if target == 'llama-tokenize':
        report['vocabulary_sha256'] = digest(source/'models/ggml-vocab-qwen2.gguf')
    filename = 'build.json' if target == 'llama-server' else 'tokenizer-build.json'
    (PRIVATE/filename).write_text(json.dumps(report, indent=2)+'\n')
    print('Pinned CPU-only', target, 'built; no GPU or upstream tool subprocess support', flush=True)

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=['download', 'build'])
    parser.add_argument('--source-checkout', type=Path)
    parser.add_argument('--target', choices=['llama-server','llama-tokenize'], default='llama-server')
    parser.add_argument('--download-source', choices=['huggingface','modelscope'], default='huggingface')
    args = parser.parse_args()
    PRIVATE.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (PRIVATE/(args.action+'.lock')).open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if args.action == 'download': download(args.download_source)
        else: build(args.source_checkout, args.target)
if __name__ == '__main__':
    main()
