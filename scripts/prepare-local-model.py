#!/usr/bin/env python3
"""Build pinned CPU-only llama.cpp and verify the official free test-model artifact."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
PRIVATE = ROOT/'.local/model-runtime'
LOCK = json.loads((ROOT/'config/local-model.lock.json').read_text())

def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024*1024), b''):
            h.update(chunk)
    return h.hexdigest()

def download():
    model = LOCK['model']
    destination = PRIVATE/(model['sha256']+'.gguf')
    if destination.exists() and destination.stat().st_size == model['bytes'] and digest(destination) == model['sha256']:
        print('Pinned model already verified', flush=True)
        return
    url = 'https://huggingface.co/'+model['repository']+'/resolve/'+model['revision']+'/'+model['file']
    temporary = destination.with_suffix('.part')
    for attempt in range(3):
        try:
            with urllib.request.urlopen(url, timeout=30) as response, temporary.open('wb') as file:
                total, h = 0, hashlib.sha256()
                while True:
                    block = response.read(1024*1024)
                    if not block:
                        break
                    total += len(block)
                    if total > model['bytes']:
                        raise ValueError('Model larger than pinned manifest')
                    file.write(block)
                    h.update(block)
                file.flush()
                os.fsync(file.fileno())
            if total != model['bytes'] or h.hexdigest() != model['sha256']:
                raise ValueError('Model size/digest mismatch; not activated')
            os.replace(temporary, destination)
            print('Official model downloaded and SHA-256 verified:', total, 'bytes', flush=True)
            return
        except (OSError, ValueError) as error:
            temporary.unlink(missing_ok=True)
            print('Transfer failure class:', type(error).__name__, flush=True)
            if attempt == 2:
                raise SystemExit('Official model download failed after three attempts; no mirror fallback')
            print('Official model transfer failed; bounded retry', attempt+1, flush=True)
            time.sleep(2**attempt)

def build(checkout):
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
    subprocess.run(['cmake', '--build', str(output), '--target', 'llama-server', '-j', '2'], env=environment, check=True)
    binary = output/'bin/llama-server'
    report = {'backend_commit': commit, 'source_tree_sha256': source_hash.hexdigest(), 'executable_sha256': digest(binary),
              'model_sha256': LOCK['model']['sha256'], 'gpu_enabled': False}
    (PRIVATE/'build.json').write_text(json.dumps(report, indent=2)+'\n')
    print('Pinned CPU-only llama-server built; no GPU or upstream tool subprocess support', flush=True)

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=['download', 'build'])
    parser.add_argument('--source-checkout', type=Path)
    args = parser.parse_args()
    PRIVATE.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (PRIVATE/(args.action+'.lock')).open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if args.action == 'download': download()
        else: build(args.source_checkout)
if __name__ == '__main__':
    main()
