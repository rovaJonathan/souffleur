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

APP_NAME = "souffleur"
LEGACY_APP_NAME = "piper-tts-gui"
"""Ancien nom du répertoire de données, migré au premier lancement."""


def _platform_dir(app_name: str) -> Path:
    """Emplacement standard des données pour cette plateforme (pas de création)."""
    xdg = os.environ.get("XDG_DATA_HOME")
    if xdg:
        return Path(xdg) / app_name
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / app_name
    if os.name == "nt":
        return Path(os.environ.get("APPDATA", Path.home())) / app_name
    return Path.home() / ".local" / "share" / app_name


def app_data_dir() -> Path:
    """Répertoire de données de l'application, créé si nécessaire."""
    base = _platform_dir(APP_NAME)
    if not base.exists():
        _migrate_legacy(base)
    base.mkdir(parents=True, exist_ok=True)
    return base


def _migrate_legacy(base: Path) -> None:
    """Reprend les données de l'ancien nom : évite de retélécharger les voix."""
    legacy = _platform_dir(LEGACY_APP_NAME)
    if not legacy.is_dir():
        return
    try:
        base.parent.mkdir(parents=True, exist_ok=True)
        legacy.rename(base)
    except OSError:
        # Migration impossible : l'appli repart simplement sur un dossier vide.
        pass


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
