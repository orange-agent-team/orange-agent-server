"""Executed twice inside the pinned Linux target image with no network or host filesystem mounts."""
import os
from pathlib import Path
import platform
import subprocess
import sys

if sys.version_info[:3]!=(3,12,13) or platform.machine()!="x86_64" or platform.libc_ver()!=("glibc","2.36") or os.geteuid()==0:
    raise RuntimeError("Unexpected installer environment")
root=Path("/tmp/input")
prefix=Path("/tmp/output");prefix.mkdir(mode=0o755)
subprocess.run([sys.executable,"-I","-B","-m","pip","--isolated","--disable-pip-version-check","--no-cache-dir","install",
                "--no-index","--no-deps","--no-compile","--ignore-installed","--only-binary=:all:","--require-hashes","--find-links",root,
                "--prefix",prefix,"-r",root/"requirements.txt"],check=True)
env=dict(os.environ,PYTHONNOUSERSITE="1",PYTHONPATH=str(prefix/"lib/python3.12/site-packages"))
subprocess.run([sys.executable,"-B","-m","pip","--disable-pip-version-check","check"],env=env,check=True)
