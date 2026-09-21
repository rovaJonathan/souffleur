"""Logique TTS : catalogue de voix, téléchargement des modèles, synthèse Piper.

Ce module ne connaît rien de Tkinter : il est utilisable seul (script, tests).

Principes :
  * le texte est découpé en segments (paragraphes -> phrases -> groupes courts)
    pour que la génération démarre vite et reste fluide sur les longs textes ;
  * la synthèse est exposée sous forme de générateur, donc consommable en
    streaming par le lecteur audio sans jamais tout garder en mémoire.
"""

from __future__ import annotations

import re
import threading
import urllib.request
import wave
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable, Iterator, List, Optional, Tuple

import numpy as np
from piper import PiperVoice, SynthesisConfig

from settings import voices_dir

# --------------------------------------------------------------------------- #
# Catalogue de voix
# --------------------------------------------------------------------------- #


@dataclass(frozen=True)
class Voice:
    """Une voix Piper du dépôt Hugging Face `rhasspy/piper-voices`."""

    key: str  # ex. "fr_FR-siwis-medium"
    label: str  # libellé affiché dans l'interface


VOICES: List[Voice] = [
    Voice("fr_FR-siwis-medium", "Français — Siwis (medium)"),
    Voice("fr_FR-upmc-medium", "Français — UPMC (medium)"),
    Voice("en_US-lessac-medium", "Anglais US — Lessac (medium)"),
    Voice("en_US-amy-medium", "Anglais US — Amy (medium)"),
    Voice("en_GB-alba-medium", "Anglais GB — Alba (medium)"),
]

DEFAULT_VOICE = "fr_FR-siwis-medium"

VOICES_BY_KEY = {voice.key: voice for voice in VOICES}

# Découpage d'une clé de voix : fr_FR-siwis-medium -> fr / fr_FR / siwis / medium
VOICE_PATTERN = re.compile(
    r"^(?P<lang_family>[^-_]+)_(?P<lang_region>[^-]+)-(?P<name>[^-]+)-(?P<quality>.+)$"
)

# Arborescence du dépôt Hugging Face (identique à celle utilisée par Piper).
URL_FORMAT = (
    "https://huggingface.co/rhasspy/piper-voices/resolve/main/"
    "{lang_family}/{lang_code}/{name}/{quality}/{lang_code}-{name}-{quality}{ext}"
)


def voice_label(key: str) -> str:
    voice = VOICES_BY_KEY.get(key)
    return voice.label if voice else key


def voice_paths(key: str) -> Tuple[Path, Path]:
    """Retourne (modèle .onnx, config .onnx.json) pour une voix donnée."""
    base = voices_dir()
    return base / f"{key}.onnx", base / f"{key}.onnx.json"


def is_voice_available(key: str) -> bool:
    """Vrai si le modèle est déjà téléchargé localement."""
    model, config = voice_paths(key)
    return model.is_file() and model.stat().st_size > 0 and config.is_file()


def _voice_urls(key: str) -> Tuple[str, str]:
    match = VOICE_PATTERN.match(key)
    if not match:
        raise ValueError(f"Nom de voix invalide : {key!r}")
    fields = {
        "lang_family": match["lang_family"],
        "lang_code": f"{match['lang_family']}_{match['lang_region']}",
        "name": match["name"],
        "quality": match["quality"],
    }
    return (
        URL_FORMAT.format(ext=".onnx", **fields),
        URL_FORMAT.format(ext=".onnx.json", **fields),
    )


ProgressCallback = Callable[[float, str], None]


def download_voice(
    key: str,
    progress: Optional[ProgressCallback] = None,
    cancel: Optional[threading.Event] = None,
) -> None:
    """Télécharge modèle + config depuis Hugging Face.

    `progress` est appelé avec (fraction entre 0 et 1, message). Le
    téléchargement se fait dans un fichier `.part` renommé à la fin, pour ne
    jamais laisser un modèle tronqué sur le disque.
    """
    model_url, config_url = _voice_urls(key)
    model_path, config_path = voice_paths(key)

    def report(fraction: float, message: str) -> None:
        if progress is not None:
            progress(fraction, message)

    # La config est minuscule : on la récupère d'abord, sans détail de progression.
    report(0.0, f"Téléchargement de la configuration de {key}…")
    _download_file(config_url, config_path, cancel=cancel)

    report(0.0, f"Téléchargement du modèle {key}…")
    _download_file(
        model_url,
        model_path,
        cancel=cancel,
        progress=lambda done, total: report(
            done / total if total else 0.0,
            f"Téléchargement de {key} — {done / 1e6:.1f} Mo"
            + (f" / {total / 1e6:.1f} Mo" if total else ""),
        ),
    )
    report(1.0, f"Voix {key} prête.")


def _download_file(
    url: str,
    destination: Path,
    cancel: Optional[threading.Event] = None,
    progress: Optional[Callable[[int, int], None]] = None,
) -> None:
    temp = destination.with_suffix(destination.suffix + ".part")
    request = urllib.request.Request(url, headers={"User-Agent": "piper-tts-gui"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            total = int(response.headers.get("Content-Length") or 0)
            done = 0
            with open(temp, "wb") as handle:
                while True:
                    if cancel is not None and cancel.is_set():
                        raise RuntimeError("Téléchargement annulé.")
                    block = response.read(256 * 1024)
                    if not block:
                        break
                    handle.write(block)
                    done += len(block)
                    if progress is not None:
                        progress(done, total)
        temp.replace(destination)
    finally:
        temp.unlink(missing_ok=True)


# --------------------------------------------------------------------------- #
# Découpage du texte
# --------------------------------------------------------------------------- #

MAX_SEGMENT_CHARS = 350
"""Taille cible d'un segment. Assez court pour un premier son rapide, assez
long pour garder une prosodie naturelle."""

# Fin de phrase : ponctuation forte, éventuels guillemets/parenthèses, espace.
_SENTENCE_END = re.compile(r"(?<=[.!?…])[\"'»)\]]*\s+")
# Ponctuation faible, utilisée seulement pour casser une phrase trop longue.
_SOFT_BREAK = re.compile(r"(?<=[,;:])\s+")


def split_text(text: str, max_chars: int = MAX_SEGMENT_CHARS) -> List[str]:
    """Découpe le texte en segments synthétisables l'un après l'autre."""
    segments: List[str] = []
    for paragraph in re.split(r"\n\s*\n", text.strip()):
        paragraph = " ".join(paragraph.split())  # normalise espaces et retours ligne
        if not paragraph:
            continue
        buffer = ""
        for sentence in _sentences(paragraph, max_chars):
            if not buffer:
                buffer = sentence
            elif len(buffer) + 1 + len(sentence) <= max_chars:
                buffer = f"{buffer} {sentence}"
            else:
                segments.append(buffer)
                buffer = sentence
        if buffer:
            segments.append(buffer)
    return segments


def _sentences(paragraph: str, max_chars: int) -> Iterator[str]:
    for sentence in _SENTENCE_END.split(paragraph):
        sentence = sentence.strip()
        if not sentence:
            continue
        if len(sentence) <= max_chars:
            yield sentence
        else:
            yield from _wrap_long_sentence(sentence, max_chars)


def _wrap_long_sentence(sentence: str, max_chars: int) -> Iterator[str]:
    """Phrase sans ponctuation forte : on casse sur virgules, sinon sur les mots."""
    for piece in _SOFT_BREAK.split(sentence):
        piece = piece.strip()
        if not piece:
            continue
        if len(piece) <= max_chars:
            yield piece
            continue
        buffer = ""
        for word in piece.split(" "):
            if not buffer:
                buffer = word
            elif len(buffer) + 1 + len(word) <= max_chars:
                buffer = f"{buffer} {word}"
            else:
                yield buffer
                buffer = word
        if buffer:
            yield buffer


# --------------------------------------------------------------------------- #
# Moteur de synthèse
# --------------------------------------------------------------------------- #

SEGMENT_GAP_MS = 120
"""Silence inséré entre deux segments, pour éviter un enchaînement abrupt."""

AudioChunkTuple = Tuple[int, np.ndarray]  # (fréquence d'échantillonnage, int16 mono)


class PiperEngine:
    """Charge les voix Piper (avec cache) et produit de l'audio PCM 16 bits."""

    def __init__(self) -> None:
        self._voices: dict[str, PiperVoice] = {}
        self._lock = threading.Lock()

    def load(self, key: str) -> PiperVoice:
        """Charge la voix, ou la retourne depuis le cache (chargement ~1 s)."""
        with self._lock:
            voice = self._voices.get(key)
            if voice is not None:
                return voice
            model_path, config_path = voice_paths(key)
            if not is_voice_available(key):
                raise FileNotFoundError(
                    f"Modèle {key} absent : téléchargez-le avant de lancer la synthèse."
                )
            voice = PiperVoice.load(model_path, config_path=config_path)
            self._voices[key] = voice
            return voice

    def synthesize_segment(
        self, key: str, segment: str, syn_config: Optional[SynthesisConfig] = None
    ) -> Iterator[AudioChunkTuple]:
        """Synthétise un segment ; Piper peut rendre plusieurs chunks (1 par phrase)."""
        voice = self.load(key)
        for chunk in voice.synthesize(segment, syn_config=syn_config):
            yield chunk.sample_rate, chunk.audio_int16_array

    def synthesize_text(
        self,
        key: str,
        text: str,
        stop_event: Optional[threading.Event] = None,
        on_segment: Optional[Callable[[int, int, str], None]] = None,
        syn_config: Optional[SynthesisConfig] = None,
    ) -> Iterator[AudioChunkTuple]:
        """Génère l'audio de tout le texte, segment par segment, en streaming.

        `on_segment(index, total, segment)` est appelé avant chaque segment, ce
        qui permet à l'interface d'afficher une progression réelle.
        `stop_event` est vérifié entre chaque chunk : l'arrêt est quasi immédiat.
        """
        segments = split_text(text)
        total = len(segments)
        sample_rate: Optional[int] = None

        for index, segment in enumerate(segments):
            if stop_event is not None and stop_event.is_set():
                return
            if on_segment is not None:
                on_segment(index, total, segment)

            # Silence de liaison entre deux segments (pas avant le premier).
            if index > 0 and sample_rate:
                yield sample_rate, _silence(sample_rate, SEGMENT_GAP_MS)

            for rate, samples in self.synthesize_segment(key, segment, syn_config):
                if stop_event is not None and stop_event.is_set():
                    return
                sample_rate = rate
                yield rate, samples


def _silence(sample_rate: int, milliseconds: int) -> np.ndarray:
    return np.zeros(int(sample_rate * milliseconds / 1000), dtype=np.int16)


def write_wav(path: str | Path, sample_rate: int, samples: np.ndarray) -> None:
    """Écrit un WAV mono 16 bits (format natif de Piper)."""
    with wave.open(str(path), "wb") as wav_file:
        wav_file.setnchannels(1)
        wav_file.setsampwidth(2)
        wav_file.setframerate(sample_rate)
        wav_file.writeframes(samples.astype(np.int16).tobytes())


def concatenate(chunks: Iterable[AudioChunkTuple]) -> Tuple[int, np.ndarray]:
    """Assemble des chunks en un seul tableau (utilisé pour l'export WAV)."""
    sample_rate = 0
    parts: List[np.ndarray] = []
    for rate, samples in chunks:
        sample_rate = rate
        parts.append(samples)
    if not parts:
        return 0, np.zeros(0, dtype=np.int16)
    return sample_rate, np.concatenate(parts)
