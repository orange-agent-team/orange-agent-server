"""Re-encode the pinned recorded sample for decoder qualification; invoked in the locked ASR venv."""

import hashlib
import json
import sys
from pathlib import Path
import av

source = Path(sys.argv[1])
out = Path(sys.argv[2])
rows = []
for suffix, container, codec, rate, layout in [
    ("flac", "flac", "flac", 44100, "stereo"),
    ("mp3", "mp3", "libmp3lame", 44100, "mono"),
    ("ogg", "ogg", "libopus", 48000, "mono"),
    ("m4a", "mp4", "aac", 44100, "mono"),
]:
    dest = out / ("recorded." + suffix)
    with (
        av.open(str(source), mode="r") as src,
        av.open(str(dest), mode="w", format=container) as target,
    ):
        stream = target.add_stream(codec, rate=rate)
        stream.layout = layout
        format_name = av.Codec(codec, "w").audio_formats[0].name
        resampler = av.AudioResampler(format=format_name, layout=layout, rate=rate)
        for frame in src.decode(audio=0):
            frame.pts = None
            for converted in resampler.resample(frame):
                for packet in stream.encode(converted):
                    target.mux(packet)
        for converted in resampler.resample(None):
            for packet in stream.encode(converted):
                target.mux(packet)
        for packet in stream.encode(None):
            target.mux(packet)
    # Count decoded source frames separately from the E09 mono/16 kHz preprocessing path.
    with av.open(str(dest), mode="r") as src:
        count = sum(frame.samples for frame in src.decode(audio=0))
    if not 10.95 * rate <= count <= 11.1 * rate:
        raise ValueError("Recorded audio lost during fixture conversion")
    rows.append(
        {
            "file": dest.name,
            "format": container if container != "mp4" else "mov",
            "source_frames": count,
            "source_rate": rate,
            "sha256": hashlib.sha256(dest.read_bytes()).hexdigest(),
        }
    )
print(json.dumps(rows))
