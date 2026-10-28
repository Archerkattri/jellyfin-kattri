"""Generate the Jellyfin Kattri app icons from the Jellyfin jellyfish mark.

The mark is the official Jellyfin jellyfish geometry, filled with a dark blood-red
gradient instead of the brand purple-to-blue one. Only the fill changes; the
outline, the eye-shaped notch and the proportions stay as published.

Outputs (all committed, so packaging does not need Python):
    packaging/linux/icons/kattri.svg     scalable icon for AppImage, deb, rpm, Flatpak
    packaging/windows/kattri.ico         multi-size icon for the MSI shortcut
    packaging/macos/AppIcon.icns         bundle icon for build-app.sh
    packaging/icons/kattri-128.rgba      raw RGBA used for the runtime window icon

Regenerate with:  python packaging/icons/build_icons.py
Requires Pillow (9.3 or newer, for ICNS writing).

The two path strings below are copied verbatim from the official jellyfin.org
logo (https://jellyfin.org/images/logo.svg, 252x72, gradient #AA5CC3 -> #00A4DC).
They are drawn in a coordinate space of roughly 76x74 units.
"""

from __future__ import annotations

import math
import re
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

ROOT = Path(__file__).resolve().parents[2]

# Body with the notch cut out (even-odd), and the small shape above it.
BODY = (
    "M.98 64.996C-3.695 55.606 26.977.001 36.5.001c9.533 0 40.153 55.713 35.527 64.995"
    "s-66.368 9.391-71.045 0m12.255-8.148c3.065 6.152 43.518 6.084 46.548 0"
    " 3.03-6.086-17.033-42.587-23.275-42.587S10.17 50.695 13.234 56.848z"
)
CREST = (
    "M24.711 49.158c-1.552-3.116 8.626-21.57 11.788-21.57 3.167.002 13.323 18.488"
    " 11.788 21.57-1.535 3.081-22.025 3.115-23.577 0z"
)

# Dark blood red, top-left to bottom-right, mirroring the brand gradient direction.
GRADIENT = ((0x6B, 0x07, 0x0C), (0xC4, 0x12, 0x18), (0xFF, 0x5A, 0x42))

TILE = 512
MARGIN = 0.10  # fraction of the tile kept clear around the mark
SUPERSAMPLE = 4

TOKEN = re.compile(r"[MmLlCcSsHhVvZzQqTtAa]|-?\d*\.?\d+(?:[eE][-+]?\d+)?")


class PathError(ValueError):
    """The source path used a command this generator does not implement."""


def _flatten_cubic(p0, p1, p2, p3, steps=24):
    points = []
    for index in range(1, steps + 1):
        t = index / steps
        u = 1.0 - t
        x = u * u * u * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t * t * t * p3[0]
        y = u * u * u * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t * t * t * p3[1]
        points.append((x, y))
    return points


def subpaths(data: str) -> list[list[tuple[float, float]]]:
    """Flatten an SVG path into closed polygons. Only the commands the mark uses are
    implemented; anything else raises rather than drawing a wrong shape."""
    tokens = TOKEN.findall(data)
    index = 0
    current = (0.0, 0.0)
    start = (0.0, 0.0)
    previous_control = None
    command = None
    polygons: list[list[tuple[float, float]]] = []
    polygon: list[tuple[float, float]] = []

    def numbers(count: int) -> list[float]:
        nonlocal index
        values = [float(tokens[index + offset]) for offset in range(count)]
        index += count
        return values

    def finish() -> None:
        nonlocal polygon
        if len(polygon) > 2:
            polygons.append(polygon)
        polygon = []

    while index < len(tokens):
        token = tokens[index]
        if token.isalpha():
            command = token
            index += 1
            if command in "Zz":
                current = start
                previous_control = None
                finish()
                continue
        if command is None:
            raise PathError("path data starts with a number")

        relative = command.islower()
        upper = command.upper()
        if upper == "M":
            x, y = numbers(2)
            if relative:
                x, y = current[0] + x, current[1] + y
            finish()
            current = start = (x, y)
            polygon = [current]
            previous_control = None
            command = "l" if relative else "L"
        elif upper == "L":
            x, y = numbers(2)
            if relative:
                x, y = current[0] + x, current[1] + y
            current = (x, y)
            polygon.append(current)
            previous_control = None
        elif upper == "H":
            (x,) = numbers(1)
            current = (current[0] + x if relative else x, current[1])
            polygon.append(current)
            previous_control = None
        elif upper == "V":
            (y,) = numbers(1)
            current = (current[0], current[1] + y if relative else y)
            polygon.append(current)
            previous_control = None
        elif upper == "C":
            x1, y1, x2, y2, x, y = numbers(6)
            if relative:
                x1, y1 = current[0] + x1, current[1] + y1
                x2, y2 = current[0] + x2, current[1] + y2
                x, y = current[0] + x, current[1] + y
            polygon.extend(_flatten_cubic(current, (x1, y1), (x2, y2), (x, y)))
            previous_control = (x2, y2)
            current = (x, y)
        elif upper == "S":
            x2, y2, x, y = numbers(4)
            if relative:
                x2, y2 = current[0] + x2, current[1] + y2
                x, y = current[0] + x, current[1] + y
            if previous_control is None:
                x1, y1 = current
            else:
                x1 = 2 * current[0] - previous_control[0]
                y1 = 2 * current[1] - previous_control[1]
            polygon.extend(_flatten_cubic(current, (x1, y1), (x2, y2), (x, y)))
            previous_control = (x2, y2)
            current = (x, y)
        else:
            raise PathError(f"unsupported path command {command!r}")
    finish()
    return polygons


def mark_polygons() -> list[list[tuple[float, float]]]:
    return subpaths(BODY) + subpaths(CREST)


def fit(polygons, size: int, margin: float):
    """Scale the mark into a square tile and return (polygons, gradient endpoints)."""
    xs = [x for polygon in polygons for x, _ in polygon]
    ys = [y for polygon in polygons for _, y in polygon]
    left, right, top, bottom = min(xs), max(xs), min(ys), max(ys)
    usable = size * (1.0 - 2.0 * margin)
    scale = usable / max(right - left, bottom - top)
    offset_x = size / 2.0 - (left + right) / 2.0 * scale
    offset_y = size / 2.0 - (top + bottom) / 2.0 * scale
    scaled = [[(x * scale + offset_x, y * scale + offset_y) for x, y in polygon] for polygon in polygons]
    return scaled, (left, top, right, bottom, scale, offset_x, offset_y)


def mask_for(polygons, size: int) -> Image.Image:
    """Even-odd fill: the notch is the second subpath of the body."""
    big = size * SUPERSAMPLE
    mask = Image.new("L", (big, big), 0)
    for polygon in polygons:
        part = Image.new("L", (big, big), 0)
        ImageDraw.Draw(part).polygon(
            [(x * SUPERSAMPLE, y * SUPERSAMPLE) for x, y in polygon], fill=255
        )
        # Hard-edged masks, so |a - b| is exactly the even-odd XOR.
        mask = ImageChops.difference(mask, part)
    return mask.resize((size, size), Image.LANCZOS)


def gradient(size: int) -> Image.Image:
    start, middle, end = GRADIENT
    image = Image.new("RGBA", (size, size))
    pixels = image.load()
    span = 2.0 * (size - 1)
    for y in range(size):
        for x in range(size):
            t = (x + y) / span
            if t < 0.5:
                a, b, local = start, middle, t * 2.0
            else:
                a, b, local = middle, end, (t - 0.5) * 2.0
            pixels[x, y] = (
                round(a[0] + (b[0] - a[0]) * local),
                round(a[1] + (b[1] - a[1]) * local),
                round(a[2] + (b[2] - a[2]) * local),
                255,
            )
    return image


def render(size: int) -> Image.Image:
    polygons, _ = fit(mark_polygons(), size, MARGIN)
    image = gradient(size)
    image.putalpha(mask_for(polygons, size))
    return image


def svg() -> str:
    polygons, (left, top, right, bottom, scale, dx, dy) = fit(
        mark_polygons(), TILE, MARGIN
    )
    del polygons
    transform = f"translate({dx:.3f} {dy:.3f}) scale({scale:.6f})"
    # The gradient runs corner to corner over the scaled mark, in tile space.
    x1, y1 = left * scale + dx, top * scale + dy
    x2, y2 = right * scale + dx, bottom * scale + dy
    start, middle, end = (f"#{r:02X}{g:02X}{b:02X}" for r, g, b in GRADIENT)
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" '
        'viewBox="0 0 512 512" role="img" aria-label="Jellyfin Kattri">\n'
        f'  <linearGradient id="blood" gradientUnits="userSpaceOnUse" '
        f'x1="{x1:.2f}" y1="{y1:.2f}" x2="{x2:.2f}" y2="{y2:.2f}">\n'
        f'    <stop offset="0" stop-color="{start}"/>\n'
        f'    <stop offset="0.5" stop-color="{middle}"/>\n'
        f'    <stop offset="1" stop-color="{end}"/>\n'
        "  </linearGradient>\n"
        f'  <g transform="{transform}" fill="url(#blood)" fill-rule="evenodd">\n'
        f'    <path d="{BODY}"/>\n'
        f'    <path d="{CREST}"/>\n'
        "  </g>\n"
        "</svg>\n"
    )


def write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    print(f"wrote {path.relative_to(ROOT)} ({len(data)} bytes)")


def main() -> None:
    # Render once at 1024 so the macOS bundle icon and the largest ICO entries are not
    # upscaled; the SVG is resolution independent and the runtime icon stays at 128.
    master = render(1024)

    write(ROOT / "packaging/linux/icons/kattri.svg", svg().encode("utf-8"))
    write(ROOT / "packaging/macos/AppIcon.icns", _icns(master))

    import io

    buffer = io.BytesIO()
    master.save(
        buffer,
        format="ICO",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    write(ROOT / "packaging/windows/kattri.ico", buffer.getvalue())

    runtime = render(128)
    write(ROOT / "packaging/icons/kattri-128.rgba", runtime.tobytes())


def _icns(master: Image.Image) -> bytes:
    import io

    buffer = io.BytesIO()
    master.save(buffer, format="ICNS")
    return buffer.getvalue()


if __name__ == "__main__":
    main()
