"""Determine the immutable local image reference accepted by classic and containerd Docker stores."""
import re
import subprocess

def local_image_id(docker,entry):
    manifest=entry["image"].rsplit("@",1)[-1]
    expected=(manifest,entry["config_digest"])
    platform_lock=entry.get("platform","linux/arm64")
    if platform_lock not in ("linux/arm64","linux/amd64"):raise ValueError("Unsupported Linux image platform")
    if not all(re.fullmatch(r"sha256:[a-f0-9]{64}",v) for v in expected):
        raise ValueError("Immutable manifest and config digests required")
    for candidate in expected:
        output=subprocess.run(docker+["image","inspect","--format","{{.Id}} {{.Os}}/{{.Architecture}}",candidate],text=True,capture_output=True)
        if output.returncode==0:
            identity,platform=output.stdout.strip().split()
            if identity in expected and platform==platform_lock:return identity
    return None
