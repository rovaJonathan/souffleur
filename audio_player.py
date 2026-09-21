"""Lecture audio en streaming via sounddevice (aucun fichier intermédiaire).

Le lecteur consomme un itérateur de chunks `(sample_rate, np.int16)` produit par
le moteur TTS et les écrit au fil de l'eau sur la sortie audio.

Règle de thread importante : PortAudio n'aime pas qu'on manipule un flux depuis
deux threads à la fois (un `abort()` pendant un `write()` bloquant peut figer le
processus). Donc **seul le thread de lecture touche le flux** ; `stop()`, lui,
se contente de lever un drapeau. Le thread de lecture écrit par petits blocs et
teste ce drapeau entre chaque bloc : l'arrêt est perçu comme immédiat
(~1 bloc, soit moins de 100 ms) sans aucun appel concurrent.
"""

from __future__ import annotations

import threading
from queue import Empty, Full, Queue
from typing import Iterable, Iterator, Optional, Tuple, TypeVar

import numpy as np
import sounddevice as sd

AudioChunkTuple = Tuple[int, np.ndarray]
T = TypeVar("T")

BLOCK_SAMPLES = 2048
"""Taille des blocs écrits sur la carte son (~93 ms à 22 050 Hz).
Compromis entre réactivité du bouton « Arrêter » et coût des appels."""


class AudioPlayer:
    """Sortie audio mono 16 bits, ouverte paresseusement au premier chunk."""

    def __init__(self) -> None:
        self._stream: Optional[sd.OutputStream] = None
        self._sample_rate: Optional[int] = None
        self._stop_event = threading.Event()

    # -- état ------------------------------------------------------------- #

    @property
    def stopped(self) -> bool:
        return self._stop_event.is_set()

    @property
    def stop_event(self) -> threading.Event:
        """Partagé avec le moteur TTS pour interrompre aussi la génération."""
        return self._stop_event

    def reset(self) -> None:
        """Réarme le drapeau d'arrêt avant une nouvelle lecture.

        Appelé depuis le thread UI *avant* de démarrer le thread de travail :
        un clic sur « Arrêter » ne peut donc jamais être effacé après coup.
        """
        self._stop_event.clear()

    # -- lecture ----------------------------------------------------------- #

    def play(self, chunks: Iterable[AudioChunkTuple]) -> None:
        """Bloquant : à appeler depuis un thread de travail, jamais depuis l'UI.

        Retourne quand tout a été joué, ou juste après un `stop()`.
        """
        try:
            for sample_rate, samples in chunks:
                if self._stop_event.is_set():
                    break
                if samples.size == 0:
                    continue
                self._ensure_stream(sample_rate)
                if not self._write_chunk(samples):
                    break
        finally:
            self._close()

    def _write_chunk(self, samples: np.ndarray) -> bool:
        """Écrit un chunk par petits blocs. Retourne False si l'arrêt est demandé."""
        block = np.ascontiguousarray(samples, dtype=np.int16)
        for start in range(0, block.size, BLOCK_SAMPLES):
            if self._stop_event.is_set():
                return False
            try:
                # Bloque le temps que le bloc soit consommé : régule
                # naturellement la génération, sans consommer de CPU.
                self._stream.write(block[start : start + BLOCK_SAMPLES])
            except sd.PortAudioError:
                return False
        return True

    def stop(self) -> None:
        """Interrompt la lecture (appelable depuis le thread Tkinter).

        Ne touche volontairement pas au flux PortAudio : c'est le thread de
        lecture qui le fermera, lui seul.
        """
        self._stop_event.set()

    # -- interne ------------------------------------------------------------ #

    def _ensure_stream(self, sample_rate: int) -> None:
        if self._stream is not None and self._sample_rate == sample_rate:
            return
        self._close()
        self._stream = sd.OutputStream(
            samplerate=sample_rate,
            channels=1,
            dtype="int16",
        )
        self._sample_rate = sample_rate
        self._stream.start()

    def _close(self) -> None:
        stream = self._stream
        self._stream = None
        self._sample_rate = None
        if stream is None:
            return
        try:
            if self._stop_event.is_set():
                stream.abort()  # arrêt demandé : on coupe net, tampon jeté
            else:
                stream.stop()  # fin normale : on laisse le tampon se vider
        except sd.PortAudioError:
            pass
        finally:
            try:
                stream.close()
            except sd.PortAudioError:
                pass


def prefetch(
    chunks: Iterable[T],
    size: int = 4,
    stop_event: Optional[threading.Event] = None,
) -> Iterator[T]:
    """Génère les chunks en avance dans un thread, via une file bornée.

    Sans cela, la synthèse du chunk N+1 ne démarrerait qu'une fois le chunk N
    entièrement joué : on entendrait un blanc entre les segments. La file
    bornée évite aussi de tout garder en mémoire sur un texte très long.

    Indifférent au type des éléments : des chunks audio nus ou accompagnés de
    leur index de segment.
    """
    queue: Queue = Queue(maxsize=size)
    sentinel = object()

    def producer() -> None:
        try:
            for item in chunks:
                while True:
                    if stop_event is not None and stop_event.is_set():
                        return
                    try:
                        queue.put(item, timeout=0.2)
                        break
                    except Full:
                        continue  # consommateur en retard : on réessaie
        except BaseException as exc:  # l'erreur est relayée au consommateur
            try:
                queue.put(exc, timeout=1.0)
            except Full:
                pass
        finally:
            try:
                queue.put(sentinel, timeout=1.0)
            except Full:
                pass

    thread = threading.Thread(target=producer, name="tts-prefetch", daemon=True)
    thread.start()

    while True:
        try:
            item = queue.get(timeout=0.2)
        except Empty:
            if stop_event is not None and stop_event.is_set():
                return
            if not thread.is_alive():
                return
            continue
        if item is sentinel:
            return
        if isinstance(item, BaseException):
            raise item
        yield item
