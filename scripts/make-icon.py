#!/usr/bin/env python3
"""Regenerates crates/mewrk/assets/icon.ico, the Windows application icon, from the drawings in crates/ui/common/assets.

The Windows counterpart of scripts/make-icon.sh. It takes the same drawings and the same split: the small drawing for the sizes up to 32 pixels,
where the shapes must stay heavy, and the full drawing above that. Each size is rendered once at 1024 pixels and scaled down, with the alpha
premultiplied so the edges of the plate don't pick up a dark fringe. Every entry is a PNG, which Windows Vista and later read directly.

The first renderer found is used, in this order: the cairosvg module, a resvg executable, rsvg-convert, ImageMagick (magick), then
headless Chrome or Edge. Pillow does the scaling and is required. Set ICON_RENDERER to one of: cairosvg, resvg, rsvg-convert, magick, chrome.
"""
import io
import os
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "crates" / "ui" / "common" / "assets"
OUTPUT = ROOT / "crates" / "mewrk" / "assets" / "icon.ico"
MASTER = 1024
SIZES = [16, 20, 24, 32, 40, 48, 64, 256]
SMALL_DRAWING_UP_TO = 32
CHROME_PATHS = [
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
]


def render_cairosvg(svg, png):
    import cairosvg  # noqa: PLC0415  (optional dependency, imported only when chosen)

    cairosvg.svg2png(url=str(svg), write_to=str(png), output_width=MASTER, output_height=MASTER)


def render_resvg(svg, png):
    subprocess.run(["resvg", "-w", str(MASTER), "-h", str(MASTER), str(svg), str(png)], check=True)


def render_rsvg(svg, png):
    subprocess.run(["rsvg-convert", "-w", str(MASTER), "-h", str(MASTER), "-o", str(png), str(svg)], check=True)


def render_magick(svg, png):
    subprocess.run(["magick", "-background", "none", "-density", "384", str(svg), "-resize", f"{MASTER}x{MASTER}", str(png)], check=True)


def render_chrome(svg, png):
    browser = next((path for path in CHROME_PATHS if os.path.exists(path)), None) or shutil.which("chrome") or shutil.which("msedge")
    if browser is None:
        raise RuntimeError("no Chrome or Edge found")
    # The page holds only the drawing, on a transparent background, at exactly the master size.
    page = png.with_suffix(".html")
    page.write_text(
        '<!doctype html><html><body style="margin:0;background:transparent">'
        f'<img src="{svg.as_uri()}" width="{MASTER}" height="{MASTER}" style="display:block"></body></html>',
        encoding="utf-8",
    )
    subprocess.run(
        [
            browser,
            "--headless=new",
            "--disable-gpu",
            "--no-sandbox",
            "--hide-scrollbars",
            "--force-device-scale-factor=1",
            f"--window-size={MASTER},{MASTER}",
            "--default-background-color=00000000",
            "--allow-file-access-from-files",
            f"--screenshot={png}",
            page.as_uri(),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=120,
    )


RENDERERS = {
    "cairosvg": render_cairosvg,
    "resvg": render_resvg,
    "rsvg-convert": render_rsvg,
    "magick": render_magick,
    "chrome": render_chrome,
}


def available(name):
    if name == "cairosvg":
        try:
            import cairosvg  # noqa: F401, PLC0415

            return True
        except (ImportError, OSError):
            return False
    if name == "resvg":
        return shutil.which("resvg") is not None
    if name == "rsvg-convert":
        return shutil.which("rsvg-convert") is not None
    if name == "magick":
        return shutil.which("magick") is not None
    return any(os.path.exists(path) for path in CHROME_PATHS) or shutil.which("chrome") is not None or shutil.which("msedge") is not None


def pick_renderer():
    forced = os.environ.get("ICON_RENDERER")
    if forced:
        if forced not in RENDERERS:
            sys.exit(f"make-icon: ICON_RENDERER must be one of {', '.join(RENDERERS)}")
        return forced
    for name in RENDERERS:
        if available(name):
            return name
    sys.exit("make-icon: no SVG renderer found (install cairosvg, resvg, rsvg-convert or ImageMagick, or Chrome or Edge)")


def render_master(renderer, svg, work):
    png = work / (svg.stem + f"-{MASTER}.png")
    RENDERERS[renderer](svg, png)
    image = Image.open(png).convert("RGBA")
    if image.size != (MASTER, MASTER):
        raise RuntimeError(f"{renderer} wrote {image.size} for {svg.name}, not {MASTER}x{MASTER}")
    return image


def scaled(master, size):
    # Premultiplied resampling: without it, the colour of the transparent margin bleeds into the plate's anti-aliased edge.
    return master.convert("RGBa").resize((size, size), Image.Resampling.LANCZOS).convert("RGBA")


def encode_ico(entries):
    """entries: list of (size, png bytes). Writes an ICONDIR with PNG-compressed images, 256 stored as 0."""
    header = struct.pack("<HHH", 0, 1, len(entries))
    directory = b""
    offset = len(header) + 16 * len(entries)
    body = b""
    for size, data in entries:
        dim = 0 if size >= 256 else size
        directory += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
        body += data
    return header + directory + body


def main():
    renderer = pick_renderer()
    print(f"make-icon: rendering with {renderer}")
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        full = render_master(renderer, ASSETS / "mewrk-icon.svg", work)
        small = render_master(renderer, ASSETS / "mewrk-icon-small.svg", work)
        entries = []
        for size in SIZES:
            source = small if size <= SMALL_DRAWING_UP_TO else full
            buffer = io.BytesIO()
            scaled(source, size).save(buffer, "PNG", optimize=True)
            entries.append((size, buffer.getvalue()))
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_bytes(encode_ico(entries))
    print(f"make-icon: wrote {OUTPUT} with sizes {', '.join(str(size) for size in SIZES)}")


if __name__ == "__main__":
    main()
