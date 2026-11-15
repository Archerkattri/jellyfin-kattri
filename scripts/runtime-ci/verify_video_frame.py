#!/usr/bin/env python3
"""Validate a bounded RGB/RGBA PNG read back from the host's video framebuffer."""

from __future__ import annotations

import hashlib
import sys
import zlib
from pathlib import Path

MAX_FILE_BYTES = 16 * 1024 * 1024
MAX_PIXELS = 4_000_000
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def paeth(left: int, above: int, upper_left: int) -> int:
    predictor = left + above - upper_left
    distances = (abs(predictor - left), abs(predictor - above), abs(predictor - upper_left))
    return (left, above, upper_left)[distances.index(min(distances))]


def decode_png(path: Path) -> tuple[int, int, int, bytes]:
    data = path.read_bytes()
    if not data or len(data) > MAX_FILE_BYTES:
        raise ValueError("frame PNG is empty or exceeds the 16 MiB bound")
    if not data.startswith(PNG_SIGNATURE):
        raise ValueError("frame capture is not a PNG")

    offset = len(PNG_SIGNATURE)
    width = height = color_type = bit_depth = None
    compressed = bytearray()
    ended = False
    while offset + 12 <= len(data):
        length = int.from_bytes(data[offset : offset + 4], "big")
        kind = data[offset + 4 : offset + 8]
        chunk_end = offset + 12 + length
        if chunk_end > len(data):
            raise ValueError("PNG chunk exceeds the file bound")
        body = data[offset + 8 : offset + 8 + length]
        declared_crc = int.from_bytes(data[offset + 8 + length : chunk_end], "big")
        if zlib.crc32(kind + body) & 0xFFFFFFFF != declared_crc:
            raise ValueError("PNG chunk checksum is invalid")
        if kind == b"IHDR":
            if width is not None or length != 13:
                raise ValueError("invalid PNG header")
            width = int.from_bytes(body[0:4], "big")
            height = int.from_bytes(body[4:8], "big")
            bit_depth, color_type = body[8], body[9]
            if body[10:] != b"\x00\x00\x00":
                raise ValueError("unsupported PNG compression, filter, or interlace mode")
        elif kind == b"IDAT":
            compressed.extend(body)
        elif kind == b"IEND":
            ended = True
            break
        offset = chunk_end

    if not ended or width is None or height is None or bit_depth != 8 or color_type not in (2, 6):
        raise ValueError("incomplete or unsupported PNG format")
    if width < 1 or height < 1 or width * height > MAX_PIXELS:
        raise ValueError("frame dimensions exceed the 4-megapixel bound")

    channels = 3 if color_type == 2 else 4
    stride = width * channels
    expected = height * (stride + 1)
    inflater = zlib.decompressobj()
    raster = inflater.decompress(bytes(compressed), expected + 1)
    if len(raster) > expected or inflater.unconsumed_tail:
        raise ValueError("PNG pixel data exceeds its declared dimensions")
    raster += inflater.flush()
    if len(raster) != expected or inflater.unused_data:
        raise ValueError("PNG pixel data has an unexpected size")

    rows = bytearray(height * stride)
    source = 0
    for y in range(height):
        filter_type = raster[source]
        source += 1
        row_start = y * stride
        prior_start = (y - 1) * stride
        for index in range(stride):
            value = raster[source + index]
            left = rows[row_start + index - channels] if index >= channels else 0
            above = rows[prior_start + index] if y else 0
            upper_left = rows[prior_start + index - channels] if y and index >= channels else 0
            if filter_type == 1:
                value += left
            elif filter_type == 2:
                value += above
            elif filter_type == 3:
                value += (left + above) // 2
            elif filter_type == 4:
                value += paeth(left, above, upper_left)
            elif filter_type != 0:
                raise ValueError("PNG uses an unknown row filter")
            rows[row_start + index] = value & 0xFF
        source += stride
    return width, height, channels, bytes(rows)


def main() -> int:
    if len(sys.argv) != 2:
        raise SystemExit("usage: verify_video_frame.py <video-screenshot.png>")
    path = Path(sys.argv[1])
    width, height, channels, pixels = decode_png(path)
    color_bins: set[tuple[int, int, int]] = set()
    visible_pixels = 0
    for offset in range(0, len(pixels), channels):
        red, green, blue = pixels[offset : offset + 3]
        if max(red, green, blue) > 16:
            visible_pixels += 1
        if len(color_bins) < 4096:
            color_bins.add((red >> 4, green >> 4, blue >> 4))

    if visible_pixels < 1024 or len(color_bins) < 12:
        raise ValueError("framebuffer capture is blank or lacks the synthetic color pattern")
    file_bytes = path.stat().st_size
    print(f"frame_width={width}")
    print(f"frame_height={height}")
    print(f"frame_visible_pixels={visible_pixels}")
    print(f"frame_distinct_color_bins={len(color_bins)}")
    print(f"frame_size_bytes={file_bytes}")
    print(f"frame_sha256={hashlib.sha256(path.read_bytes()).hexdigest()}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as error:
        print(f"frame check failed: {error}", file=sys.stderr)
        raise SystemExit(1)
