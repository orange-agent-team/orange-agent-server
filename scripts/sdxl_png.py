"""Measure a complete fixed-template PNG independently of model progress and device telemetry."""
import hashlib
import io
import zlib

from PIL import Image

MAX_PNG = 16 << 20
SIZE = (1024, 1024)


def inspect_png(data):
    if len(data) > MAX_PNG or not data.startswith(b"\x89PNG\r\n\x1a\n"):
        raise ValueError("Invalid fixed-template PNG")
    offset = 8
    has_data = False
    ended = False
    while offset < len(data):
        if len(data) - offset < 12:
            raise ValueError("Incomplete PNG chunk")
        size = int.from_bytes(data[offset:offset+4], "big")
        crc_at = offset + 8 + size
        after = crc_at + 4
        if after > len(data):
            raise ValueError("Truncated PNG")
        chunk = memoryview(data)[offset+4:crc_at]
        if zlib.crc32(chunk) != int.from_bytes(data[crc_at:after], "big"):
            raise ValueError("PNG checksum mismatch")
        tag = data[offset+4:offset+8]
        if tag == b"IHDR" and offset == 8 and size == 13:
            if bytes(chunk[4:16]) != bytes.fromhex("000004000000040008020000") or chunk[16] not in (0, 1):
                raise ValueError("Invalid PNG pixel format")
        elif tag == b"IDAT" and offset > 8:
            has_data = True
        elif tag == b"IEND" and size == 0 and has_data and after == len(data):
            ended = True
        else:
            raise ValueError("Unexpected PNG chunk")
        offset = after
    if not ended:
        raise ValueError("Missing final PNG chunk")
    with Image.open(io.BytesIO(data)) as image:
        if image.format != "PNG" or image.mode != "RGB" or image.size != SIZE:
            raise ValueError("Unexpected decoded image")
        image.load()
        if all(lo == hi for lo, hi in image.getextrema()):
            raise ValueError("Constant-color candidate")
        return {"media_type":"image/png", "width":SIZE[0], "height":SIZE[1], "bytes":len(data),
                "sha256":hashlib.sha256(data).hexdigest(), "rgb_sha256":hashlib.sha256(image.tobytes()).hexdigest()}
