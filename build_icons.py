"""Génère les icônes de l'application à partir de ``assets/logo.svg``.

Produit dans ``assets/`` :

* ``icon-<taille>.png`` pour 16, 32, 64, 128, 256, 512 et 1024 px — utilisés
  par Tkinter (``iconphoto``) et comme source des autres formats ;
* ``icon.ico`` (Windows) — écrit ici même, sans dépendance : un ``.ico`` peut
  embarquer des PNG tels quels ;
* ``icon.icns`` (macOS) — via ``iconutil``, donc seulement sous macOS.

Nécessite ``rsvg-convert`` (``brew install librsvg`` / ``apt install
librsvg2-bin``). À relancer après toute modification du SVG ; les fichiers
générés sont versionnés pour que l'application tourne sans cet outil.
"""

from __future__ import annotations

import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ASSETS = Path(__file__).resolve().parent / "assets"
SVG = ASSETS / "logo.svg"

PNG_SIZES = (16, 32, 64, 128, 256, 512, 1024)
ICO_SIZES = (16, 32, 48, 64, 128, 256)
# Nom d'entrée iconset -> taille en pixels (les « @2x » sont des variantes Retina).
ICNS_ENTRIES = {
    "icon_16x16.png": 16,
    "icon_16x16@2x.png": 32,
    "icon_32x32.png": 32,
    "icon_32x32@2x.png": 64,
    "icon_128x128.png": 128,
    "icon_128x128@2x.png": 256,
    "icon_256x256.png": 256,
    "icon_256x256@2x.png": 512,
    "icon_512x512.png": 512,
    "icon_512x512@2x.png": 1024,
}


def render_png(size: int, target: Path) -> None:
    subprocess.run(
        ["rsvg-convert", "-w", str(size), "-h", str(size), str(SVG), "-o", str(target)],
        check=True,
    )


def write_ico(pngs: dict[int, bytes], target: Path) -> None:
    """Assemble un .ico contenant des images PNG (supporté depuis Windows Vista)."""
    entries = sorted(pngs.items())
    header = struct.pack("<HHH", 0, 1, len(entries))
    directory = b""
    payload = b""
    offset = len(header) + 16 * len(entries)
    for size, data in entries:
        dim = 0 if size >= 256 else size  # 0 signifie 256 dans le format .ico
        directory += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        payload += data
        offset += len(data)
    target.write_bytes(header + directory + payload)


def write_icns(target: Path) -> None:
    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "icon.iconset"
        iconset.mkdir()
        for name, size in ICNS_ENTRIES.items():
            render_png(size, iconset / name)
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(target)], check=True)


def main() -> int:
    if shutil.which("rsvg-convert") is None:
        print("rsvg-convert introuvable (brew install librsvg / apt install librsvg2-bin)", file=sys.stderr)
        return 1

    for size in PNG_SIZES:
        render_png(size, ASSETS / f"icon-{size}.png")
        print(f"assets/icon-{size}.png")

    with tempfile.TemporaryDirectory() as tmp:
        pngs = {}
        for size in ICO_SIZES:
            path = Path(tmp) / f"{size}.png"
            render_png(size, path)
            pngs[size] = path.read_bytes()
    write_ico(pngs, ASSETS / "icon.ico")
    print("assets/icon.ico")

    if sys.platform == "darwin" and shutil.which("iconutil"):
        write_icns(ASSETS / "icon.icns")
        print("assets/icon.icns")
    else:
        print("icon.icns ignoré : iconutil n'est disponible que sous macOS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
