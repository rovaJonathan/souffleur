#!/usr/bin/env python3
"""Construit Souffleur.app pour ce Mac.

Lance le Python du ``.venv`` du dépôt : on n'embarque pas Piper, donc pas de
redistribution GPL (cf. README). Le .app est installé dans ``~/Applications``.

ponytail: chemins absolus cuits dans le lanceur. Plafond : un déplacement du
dépôt ou du venv casse le .app — relancer ce script. Binaire autonome =
PyInstaller + licence GPL-3.0.
"""

from __future__ import annotations

import os
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

PROJECT = Path(__file__).resolve().parent
PYTHON = PROJECT / ".venv" / "bin" / "python"
SCRIPT = PROJECT / "main.py"
ICON = PROJECT / "assets" / "icon.icns"
BUNDLE_NAME = "Souffleur.app"
BUNDLE_ID = "app.souffleur.gui"
DIST = PROJECT / "dist" / BUNDLE_NAME
INSTALL = Path.home() / "Applications" / BUNDLE_NAME


def check() -> None:
    if not PYTHON.is_file():
        raise SystemExit(f"venv introuvable : {PYTHON} (python -m venv .venv && pip install -r requirements.txt)")
    subprocess.run(
        [str(PYTHON), "-c", "import tkinter, piper, sounddevice"],
        check=True,
    )


def write_launcher(src: Path) -> None:
    # ponytail: script plutôt qu'un binaire C — le SDK 27 casse `cc` (libSystem.tbd).
    path = os.pathsep.join(
        [
            str(PYTHON.parent),
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
        ]
    )
    src.write_text(
        "\n".join(
            [
                "#!/bin/bash",
                f"cd {shlex.quote(str(PROJECT))} || exit 127",
                f"export VIRTUAL_ENV={shlex.quote(str(PROJECT / '.venv'))}",
                f"export PATH={shlex.quote(path)}",
                "export PYTHONUNBUFFERED=1",
                f"exec {shlex.quote(str(PYTHON))} {shlex.quote(str(SCRIPT))}",
                "",
            ]
        ),
        encoding="utf-8",
    )
    src.chmod(0o755)


def write_plist(target: Path) -> None:
    icon_entry = (
        "    <key>CFBundleIconFile</key>\n    <string>AppIcon</string>\n" if ICON.is_file() else ""
    )
    target.write_text(
        f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>fr</string>
    <key>CFBundleDisplayName</key>
    <string>Souffleur</string>
    <key>CFBundleExecutable</key>
    <string>Souffleur</string>
    <key>CFBundleIdentifier</key>
    <string>{BUNDLE_ID}</string>
{icon_entry}    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Souffleur</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.utilities</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>MIT</string>
</dict>
</plist>
""",
        encoding="utf-8",
    )


def assemble(bundle: Path) -> None:
    if bundle.exists():
        shutil.rmtree(bundle)
    macos = bundle / "Contents" / "MacOS"
    resources = bundle / "Contents" / "Resources"
    macos.mkdir(parents=True)
    resources.mkdir(parents=True)

    write_plist(bundle / "Contents" / "Info.plist")
    (bundle / "Contents" / "PkgInfo").write_text("APPLSufl", encoding="ascii")
    if ICON.is_file():
        shutil.copy2(ICON, resources / "AppIcon.icns")
    write_launcher(macos / "Souffleur")


def install(bundle: Path) -> Path:
    INSTALL.parent.mkdir(parents=True, exist_ok=True)
    if INSTALL.exists():
        shutil.rmtree(INSTALL)
    shutil.copytree(bundle, INSTALL, symlinks=True)
    return INSTALL


def main() -> int:
    check()
    assemble(DIST)
    installed = install(DIST)
    print(installed)
    return 0


if __name__ == "__main__":
    sys.exit(main())
