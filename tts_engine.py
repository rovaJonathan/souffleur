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
    request = urllib.request.Request(url, headers={"User-Agent": "souffleur"})
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

# Paragraphe : au moins une ligne vide.
_PARAGRAPH_SEP = re.compile(r"\n\s*\n")
# Fin de phrase : ponctuation forte, éventuels guillemets/parenthèses (groupe 1,
# conservés dans la phrase), puis espace.
_SENTENCE_END = re.compile(r"(?<=[.!?…])([\"'»)\]]*)\s+")
# Ponctuation faible, utilisée seulement pour casser une phrase trop longue.
_SOFT_BREAK = re.compile(r"(?<=[,;:])\s+")
_WHITESPACE = re.compile(r"\s+")

Span = Tuple[int, int]
"""Intervalle [début, fin) d'indices dans le texte d'origine."""


@dataclass(frozen=True)
class Segment:
    """Un morceau de texte synthétisé d'un bloc, avec sa position d'origine.

    `text` est la version normalisée (espaces et retours à la ligne réduits à
    un espace) envoyée à Piper ; `start`/`end` situent le segment dans le texte
    fourni par l'utilisateur, pour le surligner pendant la lecture.
    """

    text: str
    start: int
    end: int


def split_text(text: str, max_chars: int = MAX_SEGMENT_CHARS) -> List[str]:
    """Découpe le texte en segments synthétisables l'un après l'autre."""
    return [segment.text for segment in segment_text(text, max_chars)]


def segment_text(text: str, max_chars: int = MAX_SEGMENT_CHARS) -> List[Segment]:
    """Comme `split_text`, mais conserve la position de chaque segment.

    Le découpage travaille sur des intervalles du texte d'origine plutôt que
    sur des copies, pour que les positions restent exactes malgré la
    normalisation des espaces.
    """
    segments: List[Segment] = []
    for paragraph in _spans(text, (0, len(text)), _PARAGRAPH_SEP):
        buffer: Optional[Span] = None
        for sentence in _sentence_spans(text, paragraph, max_chars):
            if buffer is None:
                buffer = sentence
            elif len(_normalize(text, (buffer[0], sentence[1]))) <= max_chars:
                buffer = (buffer[0], sentence[1])
            else:
                segments.append(_segment(text, buffer))
                buffer = sentence
        if buffer is not None:
            segments.append(_segment(text, buffer))
    return segments


def _segment(text: str, span: Span) -> Segment:
    return Segment(_normalize(text, span), span[0], span[1])


def _normalize(text: str, span: Span) -> str:
    """Espaces et retours à la ligne réduits à un seul espace."""
    return " ".join(text[span[0] : span[1]].split())


def _spans(text: str, span: Span, separator: re.Pattern) -> Iterator[Span]:
    """Sous-intervalles de `span` entre deux occurrences de `separator`.

    Chaque morceau est débarrassé des espaces à ses bords ; les vides sont
    ignorés. Si le séparateur a un groupe 1, son contenu reste rattaché au
    morceau précédent (guillemet fermant après le point, par exemple).
    """
    cursor, end = span
    for match in separator.finditer(text, cursor, end):
        piece_end = match.end(1) if match.re.groups else match.start()
        yield from _stripped(text, (cursor, piece_end))
        cursor = match.end()
    yield from _stripped(text, (cursor, end))


def _stripped(text: str, span: Span) -> Iterator[Span]:
    start, end = span
    while start < end and text[start].isspace():
        start += 1
    while end > start and text[end - 1].isspace():
        end -= 1
    if start < end:
        yield start, end


def _sentence_spans(text: str, paragraph: Span, max_chars: int) -> Iterator[Span]:
    for sentence in _spans(text, paragraph, _SENTENCE_END):
        if len(_normalize(text, sentence)) <= max_chars:
            yield sentence
        else:
            yield from _wrap_long_sentence(text, sentence, max_chars)


def _wrap_long_sentence(text: str, sentence: Span, max_chars: int) -> Iterator[Span]:
    """Phrase sans ponctuation forte : on casse sur virgules, sinon sur les mots."""
    for piece in _spans(text, sentence, _SOFT_BREAK):
        if len(_normalize(text, piece)) <= max_chars:
            yield piece
            continue
        buffer: Optional[Span] = None
        for word in _spans(text, piece, _WHITESPACE):
            if buffer is None:
                buffer = word
            elif len(_normalize(text, (buffer[0], word[1]))) <= max_chars:
                buffer = (buffer[0], word[1])
            else:
                yield buffer
                buffer = word
        if buffer is not None:
            yield buffer


# --------------------------------------------------------------------------- #
# Moteur de synthèse
# --------------------------------------------------------------------------- #

SEGMENT_GAP_MS = 120
"""Silence inséré entre deux segments, pour éviter un enchaînement abrupt."""

AudioChunkTuple = Tuple[int, np.ndarray]  # (fréquence d'échantillonnage, int16 mono)

SegmentCallback = Callable[[int, int, Segment], None]
"""Appelé avec (index, total, segment) avant la synthèse de chaque segment."""

SPEED_MIN, SPEED_MAX = 0.5, 2.0
DEFAULT_SPEED = 1.0
"""Vitesse de lecture, en multiple de la cadence naturelle de la voix."""

VOLUME_MIN, VOLUME_MAX = 0.0, 1.0
DEFAULT_VOLUME = 1.0
"""Volume entre 0 et 1. Piper normalise l'audio avant d'appliquer ce facteur,
puis écrête à ±1 : monter au-dessus de 1 ne ferait que saturer."""


def _clamp(value: float, low: float, high: float) -> float:
    return max(low, min(high, value))


def synthesis_config(
    voice: PiperVoice, speed: float = DEFAULT_SPEED, volume: float = DEFAULT_VOLUME
) -> SynthesisConfig:
    """Traduit vitesse/volume en réglages Piper pour une voix donnée.

    Piper raisonne en `length_scale`, une *durée* : plus elle est grande, plus
    la voix est lente — d'où l'inverse. Le facteur est appliqué relativement au
    réglage du modèle, qui vaut souvent 1 mais n'y est pas tenu.
    """
    return SynthesisConfig(
        length_scale=voice.config.length_scale / _clamp(speed, SPEED_MIN, SPEED_MAX),
        volume=_clamp(volume, VOLUME_MIN, VOLUME_MAX),
    )


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

    def synthesize_segments(
        self,
        key: str,
        segments: List[Segment],
        stop_event: Optional[threading.Event] = None,
        on_segment: Optional[SegmentCallback] = None,
        speed: float = DEFAULT_SPEED,
        volume: float = DEFAULT_VOLUME,
    ) -> Iterator[Tuple[int, AudioChunkTuple]]:
        """Génère l'audio des segments en streaming, chaque chunk étant
        accompagné de l'index du segment dont il provient.

        Cet index permet à l'interface de suivre ce qui est *joué* et non ce qui
        est *généré* : avec le préchargement, la synthèse a plusieurs secondes
        d'avance sur la lecture.

        `on_segment(index, total, segment)` est appelé avant la synthèse de
        chaque segment. `stop_event` est vérifié entre chaque chunk : l'arrêt
        est quasi immédiat.

        La voix est chargée ici (et non par l'appelant) : c'est un générateur,
        donc le chargement — une seconde environ — a lieu dans le thread qui
        consomme, jamais dans celui de l'interface.
        """
        syn_config = synthesis_config(self.load(key), speed, volume)
        total = len(segments)
        sample_rate: Optional[int] = None

        for index, segment in enumerate(segments):
            if stop_event is not None and stop_event.is_set():
                return
            if on_segment is not None:
                on_segment(index, total, segment)

            # Silence de liaison entre deux segments (pas avant le premier).
            if index > 0 and sample_rate:
                yield index, (sample_rate, _silence(sample_rate, SEGMENT_GAP_MS))

            for rate, samples in self.synthesize_segment(key, segment.text, syn_config):
                if stop_event is not None and stop_event.is_set():
                    return
                sample_rate = rate
                yield index, (rate, samples)

    def synthesize_text(
        self,
        key: str,
        text: str,
        stop_event: Optional[threading.Event] = None,
        on_segment: Optional[SegmentCallback] = None,
        speed: float = DEFAULT_SPEED,
        volume: float = DEFAULT_VOLUME,
    ) -> Iterator[AudioChunkTuple]:
        """Génère l'audio de tout le texte, segment par segment, en streaming.

        Raccourci : découpe le texte puis délègue à `synthesize_segments`, sans
        l'index de segment (suffisant pour l'export ou un script).
        """
        for _index, chunk in self.synthesize_segments(
            key, segment_text(text), stop_event, on_segment, speed, volume
        ):
            yield chunk


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
