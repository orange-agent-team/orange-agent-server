"""Determine the immutable local image reference accepted by classic and containerd Docker stores."""
import re
import subprocess

def local_image_id(docker,entry):
    manifest=entry["image"].rsplit("@",1)[-1]
    expected=(manifest,entry["config_digest"])
    if not all(re.fullmatch(r"sha256:[a-f0-9]{64}",v) for v in expected):
        raise ValueError("Immutable manifest and config digests required")
    for candidate in expected:
        output=subprocess.run(docker+["image","inspect","--format","{{.Id}} {{.Os}}/{{.Architecture}}",candidate],text=True,capture_output=True)
        if output.returncode==0:
            identity,platform=output.stdout.strip().split()
            if identity in expected and platform=="linux/arm64":return identity
    return None
