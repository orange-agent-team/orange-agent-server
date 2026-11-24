#!/usr/bin/env python3.12
"""Produce and exercise an offline macOS ARM64 MCP tool release from two clean native builds."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

from platform_payload import write_archive
from rust_notices import notices

ROOT = Path(__file__).resolve().parents[1]
PROGRAM = "orange-knowledge"

def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT)

def file_sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()

def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("--vendor", type=Path, required=True, help="Verified versioned Cargo vendor source directory")
    cli.add_argument("--node", type=Path, required=True)
    cli.add_argument("--cpu-runtime", type=Path, required=True)
    args = cli.parse_args()
    if sys.version_info[:2] != (3, 12) or git("status", "--porcelain"):
        raise SystemExit("Python 3.12 and a clean committed source are required")
    commit = git("rev-parse", "HEAD").decode().strip()
    subprocess.run([sys.executable, ROOT/"scripts/build-embedding-release.py", "--profile", "knowledge", "--commit", commit], cwd=ROOT, check=True)
    native = ROOT/".local/knowledge-release"/commit
    build = json.loads((native/"native-build.json").read_bytes())
    binary = native/PROGRAM
    if (build["source_commit"] != commit or build["target"] != "aarch64-apple-darwin" or not build["reproducible"]
            or build["builds"] != [{PROGRAM:file_sha(binary)}]*2):
        raise ValueError("The two independently built executables do not match")
    def source(name):
        return git("show", commit+":"+name)
    files = {"bin/"+PROGRAM:binary.read_bytes(), "README.md":source("docs/local-selected-knowledge.md"),
             "Cargo.lock":source("Cargo.lock")}
    total, missing = notices(files, args.vendor.resolve(strict=True), files["Cargo.lock"], source)
    if missing:
        raise ValueError("Missing locked source notice")
    files["manifest.json"] = (json.dumps({"source":commit,"target":build["target"],"protocols":["2025-11-25","2026-07-28"],
                "transport":"stdio","sha256":{k:hashlib.sha256(v).hexdigest() for k,v in sorted(files.items())}},indent=2)+"\n").encode()
    dest = ROOT/".local/knowledge-distributions"
    dest.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(dir=dest,prefix=".verified-") as tmp:
        work = Path(tmp)
        first,second = work/"first.tgz",work/"second.tgz"
        write_archive(first,files,prefix=PROGRAM)
        write_archive(second,files,prefix=PROGRAM)
        sha = file_sha(first)
        if file_sha(second) != sha:raise ValueError("Archive bytes differ")
        with tarfile.open(first) as archive:archive.extractall(work,filter="data")
        installed=work/PROGRAM
        for key,content in files.items():
            if (installed/key).read_bytes()!=content:raise ValueError("Unpacked member differs")
        exe=installed/"bin"/PROGRAM
        smoke=subprocess.check_output([sys.executable,ROOT/"scripts/check-local-knowledge.py","--binary",exe,"--cpu-runtime",args.cpu_runtime.resolve()],cwd=ROOT,text=True)
        print(smoke,end="",flush=True)
        rows=[json.loads(line) for line in smoke.splitlines() if line.startswith("{")]
        [storage]=[v for v in rows if v.get("persistent_restart_read")]
        [reopened]=[v for v in rows if v.get("reopened_after_actual_model_search")]
        if not all(storage.get(key) for key in ("stale_cache_restore_rejected","authority_loss_refused","interrupted_write_recovered","revocation_survived_restart","unchanged_posting_segments_reused")):
            raise ValueError("Incomplete extracted persistence verification")
        if not reopened["answer_matched_unpublished_nonce"] or reopened["approved_tool_calls"]!=2:
            raise ValueError("Recovered model call failed")

        subprocess.run([args.node.resolve(),ROOT/"scripts/knowledge-interop/check.mjs",exe],cwd=ROOT,check=True)
        final=dest/(PROGRAM+"-macos-arm64-"+sha[:16]+".tar.gz")
        result={"source":commit,"archive":final.name,"sha256":sha,"bytes":first.stat().st_size,"target":build["target"],
                "identical_archives":2,"independent_builds":build["builds"],"locked_crates":total,"without_packaged_notice":missing,
                "official_stdio_client":"@modelcontextprotocol/client@2.4.0","persistent_qualification":storage,"reopened_model":reopened,"actual_local_cpu":True,"production_signed_or_notarized":False}
        receipt=work/"receipt.json"
        receipt.write_text(json.dumps(result,indent=2)+"\n")
        if final.exists():
            if final.is_symlink() or file_sha(final)!=sha:raise ValueError("Conflicting archive")
        else:os.link(first,final)
        os.replace(receipt,final.with_suffix(".json"))
        print(json.dumps(result),flush=True)

if __name__=="__main__":main()
