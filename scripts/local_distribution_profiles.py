"""Native programs, build switches and dependency locks in the private macOS distributions."""
from dataclasses import dataclass


@dataclass(frozen=True)
class Profile:
    programs: tuple
    cargo_args: tuple
    python_sources: tuple
    environment_lock: str = "backends/uv.lock"


PROFILES = {
    "embedding": Profile(("orange-embedding-host", "examples/local-embeddings"),
                         ("--features","embedding-host","--bin","orange-embedding-host","--example","local-embeddings"),
                         ("embedding.py","__init__.py")),
    "transcription": Profile(("orange-transcribe",), ("--bin","orange-transcribe"),
                             ("transcription.py","embedding.py","__init__.py")),
    "image": Profile(("orange-image",), ("--features","image-host","--bin","orange-image"),
                     ("image.py","__init__.py"), "backends/environments/sdxl-macos/uv.lock"),
}
