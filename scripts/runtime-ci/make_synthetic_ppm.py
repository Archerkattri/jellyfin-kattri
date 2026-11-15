#!/usr/bin/env python3
"""Write a small deterministic color pattern for the isolated CI frame check."""

from __future__ import annotations

import hashlib
import os
import sys
from pathlib import Path

WIDTH = 640
HEIGHT = 360


def main() -> int:
    if len(sys.argv) != 2:
        raise SystemExit("usage: make_synthetic_ppm.py <output-under-RUNNER_TEMP>")
    runner_temp = os.environ.get("RUNNER_TEMP")
    if not runner_temp or not Path(runner_temp).is_dir():
        raise SystemExit("fixture generation is restricted to a GitHub runner temp directory")

    output = Path(sys.argv[1]).resolve()
    allowed_root = Path(runner_temp).resolve()
    if not output.is_relative_to(allowed_root) or output.exists():
        raise SystemExit("refusing a fixture path outside RUNNER_TEMP or an existing file")

    pixels = bytearray(WIDTH * HEIGHT * 3)
    colors = ((235, 45, 35), (25, 170, 235), (40, 210, 100), (235, 190, 25))
    for y in range(HEIGHT):
        for x in range(WIDTH):
            quadrant = (y >= HEIGHT // 2) * 2 + (x >= WIDTH // 2)
            red, green, blue = colors[quadrant]
            gradient = (x + y) % 48
            offset = (y * WIDTH + x) * 3
            pixels[offset : offset + 3] = bytes(
                (
                    min(255, red + gradient // 3),
                    min(255, green + gradient // 4),
                    min(255, blue + gradient // 5),
                )
            )

    payload = f"P6\n{WIDTH} {HEIGHT}\n255\n".encode("ascii") + pixels
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(payload)
    print(f"synthetic_fixture_size_bytes={len(payload)}")
    print(f"synthetic_fixture_sha256={hashlib.sha256(payload).hexdigest()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
