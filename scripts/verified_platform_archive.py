"""Open an exact local platform package for installed integration checks."""
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]


@contextmanager
def unpack_archive(archive, expected):
    with tempfile.TemporaryDirectory() as tmp:
        # Verify and extract through one file descriptor to bind both operations.
        with archive.open("rb") as source:
            if hashlib.file_digest(source, "sha256").hexdigest()!=expected:
                raise ValueError("Package SHA-256 differs from receipt")
            source.seek(0)
            with tarfile.open(fileobj=source,mode="r:gz") as bundle:
                if any(not (member.isdir() or member.isreg()) for member in bundle.getmembers()):
                    raise ValueError("Unexpected platform archive member")
                bundle.extractall(tmp,filter="data")
        root=Path(tmp)/"orange-platform"
        subprocess.run([sys.executable,"-I",root/"verify.py"],cwd=tmp,check=True)
        yield root


@contextmanager
def binaries(directory):
    if directory is not None:
        yield directory.resolve(strict=True)
        return
    commit=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    releases=ROOT/".local/platform-distributions"
    records=(json.loads(f.read_bytes()) for f in releases.glob("*.tar.json"))
    record,=[r for r in records if r["source"]==commit and r["identical_archives"]==2]
    archive=releases/record["archive"]
    if archive.parent!=releases or archive.is_symlink():
        raise ValueError("Unexpected package path")
    with unpack_archive(archive,record["sha256"]) as root:
        print("Extracted platform",commit,record["sha256"],flush=True)
        yield root/"bin"
