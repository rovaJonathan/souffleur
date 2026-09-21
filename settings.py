"""Chemins applicatifs et préférences persistées entre deux sessions.

Les réglages sont stockés dans un simple fichier JSON, dans le répertoire de
données standard de la plateforme (pas de dépendance externe).
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any, Dict

APP_NAME = "piper-tts-gui"


def app_data_dir() -> Path:
    """Répertoire de données de l'application, créé si nécessaire."""
    xdg = os.environ.get("XDG_DATA_HOME")
    if xdg:
        base = Path(xdg) / APP_NAME
    elif sys.platform == "darwin":
        base = Path.home() / "Library" / "Application Support" / APP_NAME
    elif os.name == "nt":
        base = Path(os.environ.get("APPDATA", Path.home())) / APP_NAME
    else:
        base = Path.home() / ".local" / "share" / APP_NAME
    base.mkdir(parents=True, exist_ok=True)
    return base


def voices_dir() -> Path:
    """Répertoire où sont téléchargés les modèles de voix Piper."""
    path = app_data_dir() / "voices"
    path.mkdir(parents=True, exist_ok=True)
    return path


SETTINGS_FILE = "settings.json"


class Settings:
    """Petit conteneur clé/valeur persisté en JSON."""

    def __init__(self, data: Dict[str, Any] | None = None) -> None:
        self._data: Dict[str, Any] = data or {}

    @classmethod
    def load(cls) -> "Settings":
        path = app_data_dir() / SETTINGS_FILE
        try:
            with open(path, "r", encoding="utf-8") as handle:
                return cls(json.load(handle))
        except (OSError, ValueError):
            # Fichier absent ou corrompu : on repart sur des réglages vides.
            return cls()

    def save(self) -> None:
        path = app_data_dir() / SETTINGS_FILE
        try:
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(self._data, handle, indent=2, ensure_ascii=False)
        except OSError:
            # Une préférence non sauvegardée ne doit jamais casser l'appli.
            pass

    def get(self, key: str, default: Any = None) -> Any:
        return self._data.get(key, default)

    def set(self, key: str, value: Any) -> None:
        """Enregistre immédiatement : pas de perte si l'appli est tuée."""
        self._data[key] = value
        self.save()
