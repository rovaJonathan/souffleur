#!/usr/bin/env python3
"""Souffleur — synthèse vocale 100 % locale (Piper), interface Tkinter.

Lancement :
    pip install -r requirements.txt
    python main.py

Ce fichier ne contient que l'interface. La synthèse vit dans `tts_engine.py`,
la sortie audio dans `audio_player.py`, les préférences dans `settings.py`.

Règle de thread respectée partout : tout le travail long (téléchargement,
synthèse, lecture) tourne dans un thread de travail, et toute mise à jour de
widget passe par une file consommée par le thread Tkinter (`_ui` / `_pump`).
Attention : `widget.after()` appelé depuis un autre thread n'est pas fiable
(sous Tk 9 la callback n'est tout simplement jamais exécutée), d'où la file.
"""

from __future__ import annotations

import queue
import sys
import threading
import tkinter as tk
from tkinter import filedialog, messagebox, ttk
from typing import Callable, Iterable, Iterator, List, Optional, Tuple

from audio_player import AudioChunkTuple, AudioPlayer, prefetch
from settings import Settings, voices_dir
from tts_engine import (
    DEFAULT_SPEED,
    DEFAULT_VOICE,
    DEFAULT_VOLUME,
    SPEED_MAX,
    SPEED_MIN,
    VOICES,
    VOICES_BY_KEY,
    VOLUME_MAX,
    VOLUME_MIN,
    PiperEngine,
    Segment,
    concatenate,
    download_voice,
    is_voice_available,
    segment_text,
    write_wav,
)

APP_TITLE = "Souffleur"
WINDOW_TITLE = f"{APP_TITLE} — synthèse vocale locale"
PLACEHOLDER = (
    "Collez ou tapez votre texte ici, puis cliquez sur « Lire ».\n\n"
    "Les textes longs sont découpés automatiquement en phrases et joués à la suite."
)

SPEED_STEP = 0.05
VOLUME_STEP = 0.05
"""Pas de quantification des curseurs, appliqué au relâchement : un réglage
lisible (« 1,25 × ») et stable d'une session à l'autre."""

PAUSE_LABEL = "⏸  Pause"
RESUME_LABEL = "▶  Reprendre"

CURRENT_TAG = "current"
CURRENT_BACKGROUND = "#fff1a8"
CURRENT_FOREGROUND = "#1a1a1a"
"""Surlignage du segment en cours de lecture. La couleur du texte est fixée
avec celle du fond : en thème sombre, le texte est blanc par défaut et
disparaîtrait sur le jaune."""


class SouffleurApp(ttk.Frame):
    """Fenêtre principale."""

    def __init__(self, master: tk.Tk) -> None:
        super().__init__(master, padding=12)
        self.master: tk.Tk = master

        self.engine = PiperEngine()
        self.player = AudioPlayer()
        self.settings = Settings.load()
        self._busy = False  # une tâche longue est en cours
        self._segment_status = ""  # dernier statut de progression, restauré après une pause
        self._alive = True
        # File des mises à jour d'interface demandées par les threads de travail.
        self._ui_queue: "queue.Queue[tuple[Callable, tuple]]" = queue.Queue()

        self._build_ui()
        self._restore_preferences()
        self.grid(row=0, column=0, sticky="nsew")
        self._pump()  # démarre la boucle de traitement de la file

    # ------------------------------------------------------------------ UI --

    def _build_ui(self) -> None:
        self.master.title(WINDOW_TITLE)
        self.master.geometry("860x640")
        self.master.minsize(620, 460)
        self.master.columnconfigure(0, weight=1)
        self.master.rowconfigure(0, weight=1)

        self.columnconfigure(0, weight=1)
        self.rowconfigure(1, weight=1)

        # --- Barre du haut : sélecteur de voix -------------------------- #
        top = ttk.Frame(self)
        top.grid(row=0, column=0, sticky="ew", pady=(0, 8))
        top.columnconfigure(1, weight=1)

        ttk.Label(top, text="Voix :").grid(row=0, column=0, padx=(0, 8))

        self.voice_var = tk.StringVar()
        self.voice_combo = ttk.Combobox(
            top,
            textvariable=self.voice_var,
            state="readonly",
            values=[voice.label for voice in VOICES],
            width=34,
        )
        self.voice_combo.grid(row=0, column=1, sticky="w")
        self.voice_combo.bind("<<ComboboxSelected>>", self._on_voice_selected)

        self.voice_state_var = tk.StringVar(value="")
        ttk.Label(top, textvariable=self.voice_state_var).grid(
            row=0, column=2, padx=(12, 0), sticky="e"
        )

        # --- Zone de texte ---------------------------------------------- #
        text_frame = ttk.Frame(self)
        text_frame.grid(row=1, column=0, sticky="nsew")
        text_frame.columnconfigure(0, weight=1)
        text_frame.rowconfigure(0, weight=1)

        self.text = tk.Text(
            text_frame,
            wrap="word",
            undo=True,
            font=("TkTextFont", 13),
            padx=8,
            pady=8,
        )
        self.text.grid(row=0, column=0, sticky="nsew")
        scrollbar = ttk.Scrollbar(text_frame, orient="vertical", command=self.text.yview)
        scrollbar.grid(row=0, column=1, sticky="ns")
        self.text.configure(yscrollcommand=scrollbar.set)
        self.text.insert("1.0", PLACEHOLDER)
        self.text.tag_add("placeholder", "1.0", "end")
        self.text.tag_configure("placeholder", foreground="gray50")
        self.text.tag_configure(
            CURRENT_TAG, background=CURRENT_BACKGROUND, foreground=CURRENT_FOREGROUND
        )
        self.text.bind("<FocusIn>", self._clear_placeholder, add="+")
        self.text.bind("<Key>", self._clear_placeholder, add="+")

        # --- Vitesse et volume -------------------------------------------- #
        controls = ttk.Frame(self)
        controls.grid(row=2, column=0, sticky="ew", pady=(10, 0))
        controls.columnconfigure(1, weight=1)
        controls.columnconfigure(4, weight=1)

        self.speed_var = tk.DoubleVar(value=DEFAULT_SPEED)
        self.speed_value_var = tk.StringVar()
        self.speed_scale = self._build_slider(
            controls,
            column=0,
            text="Vitesse :",
            variable=self.speed_var,
            value_var=self.speed_value_var,
            bounds=(SPEED_MIN, SPEED_MAX),
            on_release=self._on_speed_released,
            on_move=self._show_speed,
        )

        self.volume_var = tk.DoubleVar(value=DEFAULT_VOLUME)
        self.volume_value_var = tk.StringVar()
        self.volume_scale = self._build_slider(
            controls,
            column=3,
            text="Volume :",
            variable=self.volume_var,
            value_var=self.volume_value_var,
            bounds=(VOLUME_MIN, VOLUME_MAX),
            on_release=self._on_volume_released,
            on_move=self._show_volume,
            padx=(20, 8),
        )

        # --- Boutons ----------------------------------------------------- #
        buttons = ttk.Frame(self)
        buttons.grid(row=3, column=0, sticky="ew", pady=(10, 6))
        buttons.columnconfigure(4, weight=1)

        self.play_button = ttk.Button(buttons, text="▶  Lire", command=self.on_play)
        self.play_button.grid(row=0, column=0)

        self.pause_button = ttk.Button(
            buttons, text=PAUSE_LABEL, command=self.on_pause, state="disabled"
        )
        self.pause_button.grid(row=0, column=1, padx=(6, 0))

        self.stop_button = ttk.Button(
            buttons, text="■  Arrêter", command=self.on_stop, state="disabled"
        )
        self.stop_button.grid(row=0, column=2, padx=6)

        self.export_button = ttk.Button(
            buttons, text="⬇  Exporter en WAV", command=self.on_export
        )
        self.export_button.grid(row=0, column=3)

        ttk.Button(buttons, text="Effacer", command=self.on_clear).grid(
            row=0, column=5, sticky="e"
        )

        # --- Progression + statut ---------------------------------------- #
        self.progress = ttk.Progressbar(self, mode="determinate", maximum=100)
        self.progress.grid(row=4, column=0, sticky="ew")

        self.status_var = tk.StringVar(value="Prêt.")
        ttk.Label(self, textvariable=self.status_var, foreground="gray30").grid(
            row=5, column=0, sticky="w", pady=(6, 0)
        )

        # --- Raccourcis clavier ------------------------------------------- #
        self.master.bind("<Control-Return>", lambda _event: self.on_play())
        self.master.bind("<Escape>", lambda _event: self.on_stop())
        self.master.bind("<space>", self._on_space)
        self.master.protocol("WM_DELETE_WINDOW", self.on_close)

    def _build_slider(
        self,
        parent: ttk.Frame,
        column: int,
        text: str,
        variable: tk.DoubleVar,
        value_var: tk.StringVar,
        bounds: tuple[float, float],
        on_release: Callable[[], None],
        on_move: Callable[[], None],
        padx: tuple[int, int] = (0, 8),
    ) -> ttk.Scale:
        """Libellé + curseur + valeur affichée, sur trois colonnes consécutives."""
        ttk.Label(parent, text=text).grid(row=0, column=column, padx=padx)

        scale = ttk.Scale(
            parent,
            from_=bounds[0],
            to=bounds[1],
            variable=variable,
            command=lambda _value: on_move(),
        )
        scale.grid(row=0, column=column + 1, sticky="ew")
        # Le réglage n'est enregistré qu'au relâchement : inutile de réécrire le
        # fichier de préférences à chaque pixel parcouru par le curseur.
        scale.bind("<ButtonRelease-1>", lambda _event: on_release())

        ttk.Label(parent, textvariable=value_var, width=6, anchor="e").grid(
            row=0, column=column + 2, padx=(8, 0)
        )
        return scale

    def _show_speed(self) -> None:
        self.speed_value_var.set(f"{self.speed_var.get():.2f} ×")

    def _show_volume(self) -> None:
        self.volume_value_var.set(f"{round(self.volume_var.get() * 100)} %")

    def _on_speed_released(self) -> None:
        value = _snap(self.speed_var.get(), SPEED_STEP, SPEED_MIN, SPEED_MAX)
        self.speed_var.set(value)
        self._show_speed()
        self.settings.set("speed", value)

    def _on_volume_released(self) -> None:
        value = _snap(self.volume_var.get(), VOLUME_STEP, VOLUME_MIN, VOLUME_MAX)
        self.volume_var.set(value)
        self._show_volume()
        self.settings.set("volume", value)

    def _clear_placeholder(self, _event: object = None) -> None:
        """Supprime le texte d'exemple au premier contact de l'utilisateur."""
        if "placeholder" in self.text.tag_names():
            ranges = self.text.tag_ranges("placeholder")
            if ranges:
                self.text.delete(*ranges)
            self.text.tag_delete("placeholder")

    # ------------------------------------------------------- préférences --

    def _restore_preferences(self) -> None:
        """Rétablit voix, vitesse et volume de la session précédente."""
        key = self.settings.get("voice", DEFAULT_VOICE)
        if key not in VOICES_BY_KEY:
            key = DEFAULT_VOICE
        self.voice_var.set(VOICES_BY_KEY[key].label)
        self._refresh_voice_state()
        self._warm_up_voice(key)

        self.speed_var.set(self._stored_number("speed", DEFAULT_SPEED, SPEED_MIN, SPEED_MAX))
        self.volume_var.set(
            self._stored_number("volume", DEFAULT_VOLUME, VOLUME_MIN, VOLUME_MAX)
        )
        self._show_speed()
        self._show_volume()

    def _stored_number(self, key: str, default: float, low: float, high: float) -> float:
        """Lit un réglage numérique en se méfiant d'un fichier édité à la main."""
        try:
            value = float(self.settings.get(key, default))
        except (TypeError, ValueError):
            return default
        return min(max(value, low), high)

    def current_voice_key(self) -> str:
        label = self.voice_var.get()
        for voice in VOICES:
            if voice.label == label:
                return voice.key
        return DEFAULT_VOICE

    def _on_voice_selected(self, _event: object = None) -> None:
        key = self.current_voice_key()
        self.settings.set("voice", key)
        self._refresh_voice_state()
        self._warm_up_voice(key)

    def _refresh_voice_state(self) -> None:
        key = self.current_voice_key()
        if is_voice_available(key):
            self.voice_state_var.set("● modèle installé")
        else:
            self.voice_state_var.set("○ modèle à télécharger")

    def _warm_up_voice(self, key: str) -> None:
        """Charge le modèle en arrière-plan pour que « Lire » démarre sans délai.

        Le premier chargement coûte ~0,7 s ; fait ici, au démarrage ou au choix
        d'une voix déjà téléchargée, il est terminé bien avant le clic.
        `PiperEngine.load` met en cache et est protégé par un verrou : si
        « Lire » arrive pendant le chargement, il attend simplement la fin.
        Toute erreur est ignorée : le vrai chemin d'erreur reste celui de la
        lecture, qui recharge et la signale.
        """
        if not is_voice_available(key):
            return

        def worker() -> None:
            try:
                self.engine.load(key)
            except Exception:
                pass

        threading.Thread(target=worker, name="voice-warmup", daemon=True).start()

    # ------------------------------------------------------------ actions --

    def on_play(self) -> None:
        self._run_action("play")

    def on_export(self) -> None:
        self._run_action("export")

    def on_pause(self) -> None:
        """Bascule pause / reprise de la lecture (sans effet hors lecture)."""
        if str(self.pause_button.cget("state")) == "disabled":
            return
        if self.player.toggle_pause():
            self.pause_button.configure(text=RESUME_LABEL)
            self._set_status("En pause.")
        else:
            self.pause_button.configure(text=PAUSE_LABEL)
            self._set_status(self._segment_status)

    def _on_space(self, _event: object = None) -> Optional[str]:
        """`Espace` = pause/reprise, sauf si la touche a déjà un sens ailleurs.

        Dans la zone de texte, elle saisit une espace (hors lecture : le texte
        est verrouillé pendant celle-ci). Sur un bouton, elle l'actionne déjà :
        ne pas basculer une seconde fois.
        """
        focus = self.master.focus_get()
        if isinstance(focus, ttk.Button):
            return None
        if focus is self.text and str(self.text.cget("state")) == "normal":
            return None
        self.on_pause()
        return "break"

    def on_stop(self) -> None:
        """Interrompt lecture ou export : le drapeau coupe aussi la génération."""
        if not self._busy:
            return
        self.player.stop()
        self._set_status("Arrêt demandé…")

    def on_clear(self) -> None:
        self._clear_placeholder()
        self.text.delete("1.0", "end")

    def on_close(self) -> None:
        self._alive = False  # arrête la boucle `_pump`
        self.player.stop()  # les threads de travail sont daemon : ils s'arrêteront
        self.master.destroy()

    def _run_action(self, action: str) -> None:
        """Point d'entrée commun : vérifie le texte, la voix, puis délègue."""
        if self._busy:
            return
        text = self._get_text()
        if not text:
            messagebox.showinfo("Texte vide", "Saisissez du texte à synthétiser.")
            return

        key = self.current_voice_key()
        if not is_voice_available(key):
            proceed = messagebox.askyesno(
                "Modèle de voix manquant",
                f"La voix « {VOICES_BY_KEY[key].label} » n'est pas encore téléchargée.\n\n"
                f"La télécharger maintenant depuis Hugging Face ?\n"
                f"(environ 60 Mo, stocké dans {voices_dir()})",
            )
            if not proceed:
                return
            # On relance la même action une fois le modèle disponible.
            self._start_download(key, lambda: self._run_action(action))
            return

        if action == "play":
            self._start_play(text, key)
        else:
            self._start_export(text, key)

    def _get_text(self) -> str:
        """Contenu brut de la zone de texte (vide si seul l'exemple est affiché).

        Volontairement sans `strip()` : les positions calculées par
        `segment_text` doivent rester alignées sur le contenu du widget pour
        le surlignage. Le découpage ignore de lui-même les espaces superflus.
        """
        if "placeholder" in self.text.tag_names():
            return ""
        text = self.text.get("1.0", "end-1c")  # sans le retour final ajouté par Tk
        return text if text.strip() else ""

    # ------------------------------------------------------- surlignage ---

    def _highlight(self, segment: Segment) -> None:
        """Surligne le segment et fait défiler pour le montrer en entier."""
        self.text.tag_remove(CURRENT_TAG, "1.0", "end")
        start, end = f"1.0+{segment.start}c", f"1.0+{segment.end}c"
        self.text.tag_add(CURRENT_TAG, start, end)
        self.text.see(end)
        self.text.see(start)

    def _clear_highlight(self) -> None:
        self.text.tag_remove(CURRENT_TAG, "1.0", "end")

    # ---------------------------------------------------- téléchargement --

    def _start_download(self, key: str, on_success: Optional[Callable[[], None]]) -> None:
        self._set_busy(True, stop_enabled=False)
        self.progress.configure(mode="determinate", maximum=100, value=0)
        self._set_status(f"Téléchargement de {key}…")

        def worker() -> None:
            try:
                download_voice(
                    key,
                    progress=lambda fraction, message: self._ui(
                        self._on_download_progress, fraction, message
                    ),
                )
            except Exception as exc:  # réseau coupé, 404, disque plein…
                self._ui(self._on_error, f"Téléchargement impossible : {exc}")
                return
            self._ui(self._on_download_done, key, on_success)

        threading.Thread(target=worker, name="voice-download", daemon=True).start()

    def _on_download_progress(self, fraction: float, message: str) -> None:
        self.progress.configure(value=max(0.0, min(1.0, fraction)) * 100)
        self._set_status(message)

    def _on_download_done(self, key: str, on_success: Optional[Callable[[], None]]) -> None:
        self._set_busy(False)
        self.progress.configure(value=0)
        self._refresh_voice_state()
        self._set_status(f"Voix {key} installée.")
        if on_success is not None:
            on_success()

    # --------------------------------------------------------- lecture ----

    def _start_play(self, text: str, key: str) -> None:
        segments = segment_text(text)
        # Réglages lus ici, sur le thread Tkinter : le worker ne touche à aucun
        # widget. Ils valent donc pour toute la lecture, d'où des curseurs gelés.
        speed, volume = self.speed_var.get(), self.volume_var.get()
        self.player.reset()  # réarmé ici, avant tout risque de clic « Arrêter »
        self._set_busy(True, stop_enabled=True, lock_text=True, pause_enabled=True)
        self.progress.configure(mode="indeterminate")
        self.progress.start(15)
        self._set_status("Chargement de la voix…")

        def worker() -> None:
            try:
                tagged = self.engine.synthesize_segments(
                    key,
                    segments,
                    stop_event=self.player.stop_event,
                    speed=speed,
                    volume=volume,
                )
                # prefetch : la synthèse du segment suivant tourne pendant la
                # lecture du segment courant, d'où un enchaînement sans blanc.
                # Le suivi est branché *après* la file : il reflète ce qui
                # part vers la carte son, pas ce qui est généré en avance.
                self.player.play(
                    self._track_playback(
                        prefetch(tagged, size=4, stop_event=self.player.stop_event),
                        segments,
                    )
                )
            except Exception as exc:
                self._ui(self._on_error, f"Erreur de synthèse : {exc}")
                return
            self._ui(self._on_play_done, len(segments))

        threading.Thread(target=worker, name="tts-play", daemon=True).start()

    def _track_playback(
        self, tagged: Iterable[Tuple[int, AudioChunkTuple]], segments: List[Segment]
    ) -> Iterator[AudioChunkTuple]:
        """Retire l'index de segment des chunks et signale chaque changement.

        Tourne dans le thread de lecture, juste avant l'écriture du chunk :
        le surlignage suit donc l'audio à un bloc près.
        """
        current = -1
        for index, chunk in tagged:
            if index != current:
                current = index
                self._ui(self._show_segment, "Lecture", index, len(segments), segments[index])
            yield chunk

    def _on_segment_progress(self, index: int, total: int, segment: Segment) -> None:
        """Appelé depuis le thread de synthèse : aucun accès direct aux widgets."""
        self._ui(self._show_segment, "Génération", index, total, segment)

    def _show_segment(self, verb: str, index: int, total: int, segment: Segment) -> None:
        self._segment_status = f"{verb} — segment {index + 1}/{total}"
        # En pause, le statut « En pause. » reste ; le chunk déjà en file peut
        # encore déclencher un changement de segment juste après le clic.
        if not self.player.paused:
            self._set_status(self._segment_status)
        self._highlight(segment)
        # En lecture la barre est en mode « animation » : on ne la pilote pas.
        if str(self.progress.cget("mode")) == "determinate":
            self.progress.configure(value=(index + 1) / total * 100)

    def _on_play_done(self, segment_count: int) -> None:
        self._set_busy(False)
        self._clear_highlight()
        self.progress.stop()
        self.progress.configure(mode="determinate", value=0)
        if self.player.stopped:
            self._set_status("Lecture interrompue.")
        else:
            self._set_status(f"Lecture terminée ({segment_count} segment(s)).")

    # ---------------------------------------------------------- export ----

    def _start_export(self, text: str, key: str) -> None:
        path = filedialog.asksaveasfilename(
            title="Exporter en WAV",
            defaultextension=".wav",
            initialfile="souffleur.wav",
            filetypes=[("Fichier WAV", "*.wav")],
        )
        if not path:
            return

        speed, volume = self.speed_var.get(), self.volume_var.get()
        self.player.reset()  # même drapeau : « Arrêter » annule aussi l'export
        self._set_busy(True, stop_enabled=True, lock_text=True)
        self.progress.configure(mode="determinate", maximum=100, value=0)
        self._set_status("Génération du fichier…")

        def worker() -> None:
            try:
                sample_rate, samples = concatenate(
                    self.engine.synthesize_text(
                        key,
                        text,
                        stop_event=self.player.stop_event,
                        on_segment=self._on_segment_progress,
                        speed=speed,
                        volume=volume,
                    )
                )
                if self.player.stopped or samples.size == 0:
                    self._ui(self._on_export_done, None)
                    return
                write_wav(path, sample_rate, samples)
            except Exception as exc:
                self._ui(self._on_error, f"Export impossible : {exc}")
                return
            self._ui(self._on_export_done, path)

        threading.Thread(target=worker, name="tts-export", daemon=True).start()

    def _on_export_done(self, path: Optional[str]) -> None:
        self._set_busy(False)
        self._clear_highlight()
        self.progress.configure(value=0)
        if path is None:
            self._set_status("Export annulé.")
        else:
            self._set_status(f"Fichier écrit : {path}")

    # ------------------------------------------------------------ helpers --

    def _ui(self, function: Callable, *args) -> None:
        """Demande l'exécution de `function` sur le thread Tkinter.

        Appelable depuis n'importe quel thread : on ne fait qu'empiler dans une
        file, c'est `_pump` (côté Tkinter) qui exécutera réellement l'appel.
        """
        self._ui_queue.put((function, args))

    def _pump(self) -> None:
        """Vide la file des mises à jour d'interface, 25 fois par seconde."""
        while True:
            try:
                function, args = self._ui_queue.get_nowait()
            except queue.Empty:
                break
            try:
                function(*args)
            except tk.TclError:
                return  # fenêtre en cours de destruction
        if self._alive:
            self.after(40, self._pump)

    def _set_status(self, message: str) -> None:
        self.status_var.set(message)

    def _set_busy(
        self,
        busy: bool,
        stop_enabled: bool = False,
        lock_text: bool = False,
        pause_enabled: bool = False,
    ) -> None:
        self._busy = busy
        self.pause_button.configure(
            text=PAUSE_LABEL, state="normal" if (busy and pause_enabled) else "disabled"
        )
        normal_state = "disabled" if busy else "normal"
        self.play_button.configure(state=normal_state)
        self.export_button.configure(state=normal_state)
        self.voice_combo.configure(state="disabled" if busy else "readonly")
        # Curseurs gelés pendant le travail : la vitesse et le volume sont figés
        # dans l'audio au moment de la synthèse, les bouger ne changerait rien.
        self.speed_scale.configure(state=normal_state)
        self.volume_scale.configure(state=normal_state)
        self.stop_button.configure(state="normal" if (busy and stop_enabled) else "disabled")
        # Texte verrouillé pendant lecture/export : une modification décalerait
        # le surlignage par rapport aux positions calculées au départ.
        self.text.configure(state="disabled" if (busy and lock_text) else "normal")

    def _on_error(self, message: str) -> None:
        self._set_busy(False)
        self._clear_highlight()
        self.progress.stop()
        self.progress.configure(mode="determinate", value=0)
        self._set_status("Erreur.")
        messagebox.showerror(APP_TITLE, message)


def _snap(value: float, step: float, low: float, high: float) -> float:
    """Arrondit au pas le plus proche, dans les bornes (et sans bruit flottant)."""
    return round(round(min(max(value, low), high) / step) * step, 4)


def main() -> int:
    root = tk.Tk()
    try:
        SouffleurApp(root)
    except Exception as exc:  # erreur de démarrage : on prévient proprement
        messagebox.showerror(APP_TITLE, f"Démarrage impossible : {exc}")
        return 1
    root.mainloop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
