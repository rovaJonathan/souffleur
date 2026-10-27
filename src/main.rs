// Pas de console derrière la fenêtre sous Windows (en version optimisée).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Souffleur — synthèse vocale 100 % locale (Piper), interface egui.
//!
//! Lancement :
//!     cargo run --release
//!
//! Ce fichier ne contient que l'interface. La synthèse vit dans `tts_engine`
//! (et `piper` / `espeak` pour le modèle lui-même), la sortie audio dans
//! `audio_player`, les préférences dans `settings`.
//!
//! Règle de thread respectée partout : tout le travail long (téléchargement,
//! synthèse, lecture) tourne dans un thread de travail, et toute mise à jour de
//! l'interface passe par un canal vidé au début de chaque image (`UiHandle` /
//! `pump`), le thread de travail demandant simplement un nouvel affichage.

mod audio_player;
mod espeak;
mod piper;
mod settings;
mod tts_engine;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::Result;
use eframe::egui::{
    self, Align, Color32, FontId, Key, Modifiers, RichText, TextFormat, text::CCursor,
    text::CCursorRange, text::LayoutJob,
};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

use audio_player::{AudioPlayer, prefetch};
use settings::{Settings, voices_dir};
use tts_engine::{
    DEFAULT_SPEED, DEFAULT_VOICE, DEFAULT_VOLUME, MAX_SEGMENT_CHARS, PiperEngine, SPEED_MAX,
    SPEED_MIN, VOICES, VOLUME_MAX, VOLUME_MIN, WAV_FORMAT, WavWriter, available_export_formats,
    convert_audio, download_voice, export_format_for, is_voice_available, segment_text,
    voice_by_key,
};

const APP_TITLE: &str = "Souffleur";
const WINDOW_TITLE: &str = "Souffleur — synthèse vocale locale";

/// Icône de fenêtre (barre de titre, Dock, Alt-Tab), voir `cargo xtask icons`.
const ICON_PNG: &[u8] = include_bytes!("../assets/icon-256.png");
const PLACEHOLDER: &str = "Collez ou tapez votre texte ici, puis cliquez sur « Lire ».\n\n\
     Les textes longs sont découpés automatiquement en phrases et joués à la suite.";

/// Pas de quantification des curseurs, appliqué au relâchement : un réglage
/// lisible (« 1.25 × ») et stable d'une session à l'autre.
const SPEED_STEP: f64 = 0.05;
const VOLUME_STEP: f64 = 0.05;

const OPEN_EXTENSIONS: &[&str] = &["txt", "md", "markdown", "text"];

const PAUSE_LABEL: &str = "⏸  Pause";
const RESUME_LABEL: &str = "▶  Reprendre";

/// Surlignage du segment en cours de lecture. La couleur du texte est fixée
/// avec celle du fond : en thème sombre, le texte est blanc par défaut et
/// disparaîtrait sur le jaune.
const CURRENT_BACKGROUND: Color32 = Color32::from_rgb(0xff, 0xf1, 0xa8);
const CURRENT_FOREGROUND: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1a);

const TEXT_FONT_SIZE: f32 = 16.0;
const TEXT_MARGIN: f32 = 8.0;

#[derive(Clone, Copy)]
enum Action {
    Play,
    Export,
}

/// Mises à jour d'interface demandées par les threads de travail.
enum UiMessage {
    DownloadProgress(f32, String),
    DownloadDone {
        key: &'static str,
        then: Action,
    },
    /// Segment en cours : `verb` vaut « Lecture » ou « Génération ».
    Segment {
        verb: &'static str,
        index: usize,
        total: usize,
        span: (usize, usize),
    },
    PlayDone(usize),
    ExportDone(Option<PathBuf>),
    Status(String),
    Error(String),
}

/// Côté threads de travail : empile un message et réveille l'interface.
///
/// Les threads ne touchent jamais l'état de l'interface ; c'est `pump`, au
/// début de chaque image, qui applique réellement les messages.
#[derive(Clone)]
struct UiHandle {
    sender: Sender<UiMessage>,
    ctx: egui::Context,
}

impl UiHandle {
    fn send(&self, message: UiMessage) {
        let _ = self.sender.send(message);
        self.ctx.request_repaint();
    }
}

/// Ce qui reste actif pendant une tâche longue.
#[derive(Clone, Copy, Default)]
struct Busy {
    stop_enabled: bool,
    /// Texte verrouillé : une modification décalerait le surlignage.
    lock_text: bool,
    pause_enabled: bool,
    /// Curseurs actifs (lecture) ou gelés (export, réglage figé dans le WAV).
    live_controls: bool,
}

/// Vitesse courante, partagée sans verrou avec le thread de synthèse qui la
/// relit avant chaque segment.
#[derive(Clone, Default)]
struct LiveSpeed(Arc<AtomicU64>);

impl LiveSpeed {
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn set(&self, value: f64) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

/// Fenêtre principale.
struct SouffleurApp {
    engine: Arc<PiperEngine>,
    player: AudioPlayer,
    settings: Settings,
    /// Une tâche longue est en cours, avec ce qui reste actif pendant.
    busy: Option<Busy>,
    live_speed: LiveSpeed,
    /// Dernier statut de progression, restauré après une pause.
    segment_status: String,
    ui: UiHandle,
    ui_receiver: Receiver<UiMessage>,
    export_label: &'static str,

    voice_key: &'static str,
    voice_installed: bool,
    text: String,
    speed: f64,
    volume: f64,
    paused: bool,
    status: String,
    progress: f32,
    /// Segment surligné (octets dans `text`), et défilement à faire pour le montrer.
    highlight: Option<(usize, usize)>,
    scroll_to_highlight: bool,
    /// Après ouverture d'un fichier : curseur et défilement remis au début.
    reset_view: bool,
}

impl SouffleurApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (sender, ui_receiver) = mpsc::channel();
        // « Exporter… » si ffmpeg ouvre le choix du format, sinon WAV seul.
        let export_label = if available_export_formats().len() > 1 {
            "⬇  Exporter…"
        } else {
            "⬇  Exporter en WAV"
        };
        let mut app = Self {
            engine: Arc::default(),
            player: AudioPlayer::default(),
            settings: Settings::load(),
            busy: None,
            live_speed: LiveSpeed::default(),
            segment_status: String::new(),
            ui: UiHandle {
                sender,
                ctx: cc.egui_ctx.clone(),
            },
            ui_receiver,
            export_label,
            voice_key: DEFAULT_VOICE,
            voice_installed: false,
            text: String::new(),
            speed: DEFAULT_SPEED,
            volume: DEFAULT_VOLUME,
            paused: false,
            status: "Prêt.".into(),
            progress: 0.0,
            highlight: None,
            scroll_to_highlight: false,
            reset_view: false,
        };
        app.restore_preferences();
        app
    }

    // ------------------------------------------------------------------ UI --

    fn build_ui(&mut self, ui: &mut egui::Ui) {
        self.handle_shortcuts(ui);
        let busy = self.busy;
        let idle = busy.is_none();

        // --- Barre du bas : curseurs, boutons, progression, statut -------- //
        egui::Panel::bottom("commandes")
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                ui.add_space(10.0);
                self.build_sliders(ui, idle || busy.is_some_and(|b| b.live_controls));
                ui.add_space(10.0);
                self.build_buttons(ui, busy);
                ui.add_space(6.0);
                // Au repos, egui dessinerait un point au début de la barre vide.
                let progress = if self.progress > 0.0 {
                    self.progress
                } else {
                    0.0
                };
                ui.add(egui::ProgressBar::new(progress).desired_height(6.0).fill(
                    if progress > 0.0 {
                        ui.visuals().selection.bg_fill
                    } else {
                        Color32::TRANSPARENT
                    },
                ));
                ui.add_space(6.0);
                ui.label(RichText::new(&self.status).color(ui.visuals().weak_text_color()));
            });

        // --- Barre du haut : sélecteur de voix -------------------------- //
        egui::Panel::top("voix")
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Voix :");
                    ui.add_enabled_ui(idle, |ui| {
                        let label =
                            voice_by_key(self.voice_key).map_or(self.voice_key, |v| v.label);
                        egui::ComboBox::from_id_salt("voix")
                            .selected_text(label)
                            .width(260.0)
                            .show_ui(ui, |ui| {
                                for voice in VOICES {
                                    if ui
                                        .selectable_label(self.voice_key == voice.key, voice.label)
                                        .clicked()
                                        && self.voice_key != voice.key
                                    {
                                        self.voice_key = voice.key;
                                        self.on_voice_selected();
                                    }
                                }
                            });
                    });
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.label(if self.voice_installed {
                            "modèle installé"
                        } else {
                            "modèle à télécharger"
                        });
                        voice_state_dot(ui, self.voice_installed);
                    });
                });
                ui.add_space(8.0);
            });

        // --- Zone de texte ---------------------------------------------- //
        egui::CentralPanel::no_frame().show(ui, |ui| {
            self.build_text(ui, busy.is_some_and(|b| b.lock_text))
        });
    }

    fn build_text(&mut self, ui: &mut egui::Ui, locked: bool) {
        let text_id = egui::Id::new("texte");
        if self.reset_view {
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), text_id) {
                state
                    .cursor
                    .set_char_range(Some(CCursorRange::one(CCursor::new(0))));
                // Ctrl+Z ne doit pas « défaire » l'ouverture.
                state.clear_undoer();
                state.store(ui.ctx(), text_id);
            }
            ui.memory_mut(|memory| memory.request_focus(text_id));
        }

        // Remplir la hauteur : `TextEdit` ne connaît qu'un nombre de lignes
        // (sa taille minimale ne joue que sur la largeur). Mesuré hors de la
        // zone défilante, dont la hauteur disponible est illimitée.
        let row_height =
            ui.fonts_mut(|fonts| fonts.row_height(&FontId::proportional(TEXT_FONT_SIZE)));
        let rows = ((ui.available_height() - 2.0 * TEXT_MARGIN) / row_height)
            .floor()
            .max(1.0) as usize;
        let mut scroll = egui::ScrollArea::vertical().auto_shrink(false);
        if std::mem::take(&mut self.reset_view) {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        let highlight = self.highlight;
        scroll.show(ui, |ui| {
            let mut layouter = |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
                let mut job =
                    highlighted_job(buffer.as_str(), highlight, ui.visuals().text_color());
                job.wrap.max_width = wrap_width;
                ui.fonts_mut(|fonts| fonts.layout_job(job))
            };
            let output = egui::TextEdit::multiline(&mut self.text)
                .id(text_id)
                .hint_text(PLACEHOLDER)
                .font(FontId::proportional(TEXT_FONT_SIZE))
                .margin(TEXT_MARGIN)
                .desired_width(f32::INFINITY)
                .desired_rows(rows)
                .interactive(!locked)
                .layouter(&mut layouter)
                .show(ui);

            // Défilement automatique : montre le segment en entier, son début
            // en priorité s'il est plus haut que la zone visible.
            if let Some((start, end)) =
                highlight.filter(|_| std::mem::take(&mut self.scroll_to_highlight))
            {
                let cursor_rect = |byte: usize| {
                    let index = self
                        .text
                        .get(..byte)
                        .map_or(0, |prefix| prefix.chars().count());
                    output
                        .galley
                        .pos_from_cursor(CCursor::new(index))
                        .translate(output.galley_pos.to_vec2())
                };
                let (top, bottom) = (cursor_rect(start), cursor_rect(end));
                let segment = top.union(bottom);
                let target = if segment.height() > ui.clip_rect().height() {
                    top
                } else {
                    segment
                };
                ui.scroll_to_rect(target, None);
            }
        });
    }

    fn build_sliders(&mut self, ui: &mut egui::Ui, enabled: bool) {
        ui.add_enabled_ui(enabled, |ui| {
            ui.columns(2, |columns| {
                let speed = slider_row(
                    &mut columns[0],
                    "Vitesse :",
                    &mut self.speed,
                    SPEED_MIN..=SPEED_MAX,
                    |v| format!("{v:.2} ×"),
                );
                let volume = slider_row(
                    &mut columns[1],
                    "Volume :",
                    &mut self.volume,
                    VOLUME_MIN..=VOLUME_MAX,
                    |v| format!("{} %", (v * 100.0).round()),
                );

                if speed.changed() {
                    self.show_speed();
                }
                if volume.changed() {
                    self.show_volume();
                }
                // Le réglage n'est enregistré qu'au relâchement (ou au clavier) :
                // inutile de réécrire le fichier de préférences à chaque pixel.
                if released(&speed) {
                    self.speed = snap(self.speed, SPEED_STEP, SPEED_MIN, SPEED_MAX);
                    self.show_speed();
                    self.settings.set("speed", self.speed);
                }
                if released(&volume) {
                    self.volume = snap(self.volume, VOLUME_STEP, VOLUME_MIN, VOLUME_MAX);
                    self.show_volume();
                    self.settings.set("volume", self.volume);
                }
            });
        });
    }

    fn build_buttons(&mut self, ui: &mut egui::Ui, busy: Option<Busy>) {
        let idle = busy.is_none();
        ui.horizontal(|ui| {
            if ui.add_enabled(idle, egui::Button::new("▶  Lire")).clicked() {
                self.on_play();
            }
            let pause_label = if self.paused {
                RESUME_LABEL
            } else {
                PAUSE_LABEL
            };
            if ui
                .add_enabled(
                    busy.is_some_and(|b| b.pause_enabled),
                    egui::Button::new(pause_label),
                )
                .clicked()
            {
                self.on_pause();
            }
            if ui
                .add_enabled(
                    busy.is_some_and(|b| b.stop_enabled),
                    egui::Button::new("■  Arrêter"),
                )
                .clicked()
            {
                self.on_stop();
            }
            if ui
                .add_enabled(idle, egui::Button::new(self.export_label))
                .clicked()
            {
                self.on_export();
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(
                        !busy.is_some_and(|b| b.lock_text),
                        egui::Button::new("Effacer"),
                    )
                    .clicked()
                {
                    self.on_clear();
                }
                if ui.add_enabled(idle, egui::Button::new("Ouvrir…")).clicked() {
                    self.on_open();
                }
            });
        });
    }

    /// Raccourcis : Ctrl/Cmd+O ouvrir, Ctrl/Cmd+Entrée lire, Espace pause,
    /// Échap arrêter. Traités avant les widgets, qui ne les voient donc pas
    /// (pas de saut de ligne inséré par Ctrl+Entrée dans le texte).
    fn handle_shortcuts(&mut self, ui: &mut egui::Ui) {
        let text_id = egui::Id::new("texte");
        let focused = ui.memory(|memory| memory.focused());
        let pause_enabled = self.busy.is_some_and(|b| b.pause_enabled);
        let (play, open, stop, pause) = ui.input_mut(|input| {
            let mut shortcut = |key| {
                input.consume_key(Modifiers::COMMAND, key)
                    || input.consume_key(Modifiers::CTRL, key)
            };
            let play = shortcut(Key::Enter);
            let open = shortcut(Key::O);
            let stop = input.key_pressed(Key::Escape);
            // `Espace` = pause/reprise, sauf si la touche a déjà un sens : sur un
            // bouton, elle l'actionne ; dans le texte modifiable, elle saisit
            // une espace (le texte est verrouillé pendant la lecture).
            let pause = pause_enabled
                && (focused.is_none() || focused == Some(text_id))
                && input.consume_key(Modifiers::NONE, Key::Space);
            (play, open, stop, pause)
        });
        if play {
            self.on_play();
        }
        if open {
            self.on_open();
        }
        if stop {
            self.on_stop();
        }
        if pause {
            self.on_pause();
        }
    }

    fn show_speed(&self) {
        // Copie vers un simple atomique : le thread de synthèse le lit avant
        // chaque segment, sans jamais toucher à l'état de l'interface.
        self.live_speed.set(self.speed);
    }

    fn show_volume(&self) {
        // Le lecteur applique le gain bloc par bloc : effet quasi immédiat.
        self.player.set_volume(self.volume);
    }

    // ------------------------------------------------------- préférences --

    /// Rétablit voix, vitesse et volume de la session précédente.
    fn restore_preferences(&mut self) {
        self.voice_key = self
            .settings
            .get_str("voice")
            .and_then(voice_by_key)
            .map_or(DEFAULT_VOICE, |v| v.key);
        self.refresh_voice_state();
        self.warm_up_voice(self.voice_key);

        self.speed = self.stored_number("speed", DEFAULT_SPEED, SPEED_MIN, SPEED_MAX);
        self.volume = self.stored_number("volume", DEFAULT_VOLUME, VOLUME_MIN, VOLUME_MAX);
        self.show_speed();
        self.show_volume();
    }

    /// Lit un réglage numérique en se méfiant d'un fichier édité à la main.
    fn stored_number(&self, key: &str, default: f64, low: f64, high: f64) -> f64 {
        self.settings
            .get_f64(key)
            .filter(|v| v.is_finite())
            .map_or(default, |v| v.clamp(low, high))
    }

    fn on_voice_selected(&mut self) {
        self.settings.set("voice", self.voice_key);
        self.refresh_voice_state();
        self.warm_up_voice(self.voice_key);
    }

    fn refresh_voice_state(&mut self) {
        self.voice_installed = is_voice_available(self.voice_key);
    }

    /// Charge le modèle en arrière-plan pour que « Lire » démarre sans délai.
    ///
    /// Le premier chargement coûte ~0,6 s ; fait ici, au démarrage ou au choix
    /// d'une voix déjà téléchargée, il est terminé bien avant le clic.
    /// `PiperEngine::load` met en cache et est protégé par un verrou : si
    /// « Lire » arrive pendant le chargement, il attend simplement la fin.
    /// Toute erreur est ignorée : le vrai chemin d'erreur reste celui de la
    /// lecture, qui recharge et la signale.
    fn warm_up_voice(&self, key: &'static str) {
        if !is_voice_available(key) {
            return;
        }
        let engine = self.engine.clone();
        let _ = thread::Builder::new()
            .name("voice-warmup".into())
            .spawn(move || {
                let _ = engine.load(key);
            });
    }

    // ------------------------------------------------------------ actions --

    fn on_play(&mut self) {
        self.run_action(Action::Play);
    }

    fn on_export(&mut self) {
        self.run_action(Action::Export);
    }

    /// Bascule pause / reprise de la lecture (sans effet hors lecture).
    fn on_pause(&mut self) {
        if !self.busy.is_some_and(|b| b.pause_enabled) {
            return;
        }
        self.paused = self.player.toggle_pause();
        self.status = if self.paused {
            "En pause.".into()
        } else {
            self.segment_status.clone()
        };
    }

    /// Interrompt lecture ou export : le drapeau coupe aussi la génération.
    fn on_stop(&mut self) {
        if !self.busy.is_some_and(|b| b.stop_enabled) {
            return;
        }
        self.player.stop();
        self.paused = false;
        self.status = "Arrêt demandé…".into();
    }

    fn on_clear(&mut self) {
        self.text.clear();
    }

    /// Remplace le contenu de la zone de texte par celui d'un fichier.
    fn on_open(&mut self) {
        if self.busy.is_some() {
            return; // texte verrouillé pendant lecture/export
        }
        let Some(path) = FileDialog::new()
            .set_title("Ouvrir un fichier texte")
            .add_filter("Fichiers texte", OPEN_EXTENSIONS)
            .pick_file()
        else {
            return;
        };
        match read_text_file(&path) {
            Ok(content) => {
                self.text = content;
                self.reset_view = true;
                self.status = format!("Fichier ouvert : {}", path.display());
            }
            // introuvable, droits, dossier…
            Err(error) => show_error(&format!("Ouverture impossible : {error}")),
        }
    }

    /// Point d'entrée commun : vérifie le texte, la voix, puis délègue.
    fn run_action(&mut self, action: Action) {
        if self.busy.is_some() {
            return;
        }
        // Volontairement sans `trim()` : les positions calculées par
        // `segment_text` doivent rester alignées sur le contenu affiché, pour
        // le surlignage. Le découpage ignore de lui-même les espaces superflus.
        if self.text.trim().is_empty() {
            MessageDialog::new()
                .set_level(MessageLevel::Info)
                .set_title("Texte vide")
                .set_description("Saisissez du texte à synthétiser.")
                .show();
            return;
        }

        let key = self.voice_key;
        if !is_voice_available(key) {
            let label = voice_by_key(key).map_or(key, |v| v.label);
            let proceed = MessageDialog::new()
                .set_level(MessageLevel::Warning)
                .set_title("Modèle de voix manquant")
                .set_description(format!(
                    "La voix « {label} » n'est pas encore téléchargée.\n\n\
                     La télécharger maintenant depuis Hugging Face ?\n\
                     (environ 60 Mo, stocké dans {})",
                    voices_dir().display()
                ))
                .set_buttons(MessageButtons::YesNo)
                .show();
            if proceed == MessageDialogResult::Yes {
                // On relance la même action une fois le modèle disponible.
                self.start_download(key, action);
            }
            return;
        }

        match action {
            Action::Play => self.start_play(key),
            Action::Export => self.start_export(key),
        }
    }

    // ---------------------------------------------------- téléchargement --

    fn start_download(&mut self, key: &'static str, then: Action) {
        self.set_busy(Some(Busy::default()));
        self.progress = 0.0;
        self.status = format!("Téléchargement de {key}…");

        let ui = self.ui.clone();
        spawn("voice-download", move || {
            let progress =
                |fraction, message| ui.send(UiMessage::DownloadProgress(fraction, message));
            match download_voice(key, &progress, None) {
                Ok(()) => ui.send(UiMessage::DownloadDone { key, then }),
                // réseau coupé, 404, disque plein…
                Err(error) => ui.send(UiMessage::Error(format!(
                    "Téléchargement impossible : {error:#}"
                ))),
            }
        });
    }

    fn on_download_done(&mut self, key: &str, then: Action) {
        self.set_busy(None);
        self.progress = 0.0;
        self.refresh_voice_state();
        self.status = format!("Voix {key} installée.");
        self.run_action(then);
    }

    // --------------------------------------------------------- lecture ----

    fn start_play(&mut self, key: &'static str) {
        let segments = segment_text(&self.text, MAX_SEGMENT_CHARS);
        let total = segments.len();
        // Réglages « live » : la vitesse est relue par le moteur avant chaque
        // segment, le volume appliqué par le lecteur à chaque bloc. Piper
        // synthétise donc à volume plein.
        self.player.reset(); // réarmé ici, avant tout risque de clic « Arrêter »
        self.set_busy(Some(Busy {
            stop_enabled: true,
            lock_text: true,
            pause_enabled: true,
            live_controls: true,
        }));
        self.progress = 0.0;
        self.status = "Chargement de la voix…".into();

        let (engine, player, ui, speed) = (
            self.engine.clone(),
            self.player.clone(),
            self.ui.clone(),
            self.live_speed.clone(),
        );
        spawn("tts-play", move || {
            let stop = player.stop_flag();
            let spans: Vec<(usize, usize)> = segments.iter().map(|s| (s.start, s.end)).collect();
            // prefetch : la synthèse du segment suivant tourne pendant la
            // lecture du segment courant, d'où un enchaînement sans blanc.
            let tagged = prefetch(4, stop.clone(), move |emit| {
                engine.synthesize_segments(
                    key,
                    &segments,
                    &stop,
                    None,
                    &|| speed.get(),
                    DEFAULT_VOLUME,
                    &mut |index, chunk| emit((index, chunk)),
                )
            });
            // Le suivi est branché *après* la file : il reflète ce qui part
            // vers la carte son, pas ce qui est généré en avance.
            let mut current = None;
            let tracked = tagged.map(|item| {
                item.map(|(index, chunk)| {
                    if current != Some(index) {
                        current = Some(index);
                        ui.send(UiMessage::Segment {
                            verb: "Lecture",
                            index,
                            total,
                            span: spans[index],
                        });
                    }
                    chunk
                })
            });
            match player.play(tracked) {
                Ok(()) => ui.send(UiMessage::PlayDone(total)),
                Err(error) => ui.send(UiMessage::Error(format!("Erreur de synthèse : {error:#}"))),
            }
        });
    }

    fn show_segment(&mut self, verb: &str, index: usize, total: usize, span: (usize, usize)) {
        self.segment_status = format!("{verb} — segment {}/{total}", index + 1);
        // En pause, le statut « En pause. » reste ; le chunk déjà en file peut
        // encore déclencher un changement de segment juste après le clic.
        if !self.player.paused() {
            self.status = self.segment_status.clone();
        }
        self.highlight = Some(span);
        self.scroll_to_highlight = true;
        self.progress = (index + 1) as f32 / total.max(1) as f32;
    }

    fn on_play_done(&mut self, segment_count: usize) {
        self.finish_task();
        self.status = if self.player.stopped() {
            "Lecture interrompue.".into()
        } else {
            format!("Lecture terminée ({segment_count} segment(s)).")
        };
    }

    // ---------------------------------------------------------- export ----

    fn start_export(&mut self, key: &'static str) {
        // MP3 / OGG ne sont proposés que si ffmpeg est présent ; le format
        // retenu est déduit de l'extension du nom choisi (WAV par défaut).
        let mut dialog = FileDialog::new()
            .set_title("Exporter l'audio")
            .set_file_name(format!("souffleur.{}", WAV_FORMAT.extension));
        for format in available_export_formats() {
            dialog = dialog.add_filter(format.label, &[format.extension]);
        }
        let Some(mut path) = dialog.save_file() else {
            return;
        };
        if path.extension().is_none() {
            path.set_extension(WAV_FORMAT.extension);
        }
        let export_format = export_format_for(&path);

        let (text, speed, volume) = (self.text.clone(), self.speed, self.volume);
        self.player.reset(); // même drapeau : « Arrêter » annule aussi l'export
        self.set_busy(Some(Busy {
            stop_enabled: true,
            lock_text: true,
            ..Busy::default()
        }));
        self.progress = 0.0;
        self.status = "Génération du fichier…".into();

        let (engine, ui, stop) = (
            self.engine.clone(),
            self.ui.clone(),
            self.player.stop_flag(),
        );
        spawn("tts-export", move || {
            // Pour un format compressé, le WAV intermédiaire est écrit à côté
            // du fichier final (même disque : pas de copie), puis converti.
            let wav_path = if export_format == WAV_FORMAT {
                path.clone()
            } else {
                with_suffix(&path, ".tmp.wav")
            };
            let result = (|| -> Result<Option<PathBuf>> {
                // Écriture au fil de l'eau : le fichier grandit pendant la
                // synthèse, rien n'est accumulé en mémoire. Un arrêt ou une
                // erreur laisse `WavWriter` supprimer son `.part`.
                let mut wav = WavWriter::new(&wav_path);
                let mut write_error = None;
                let on_segment = |index, total, segment: &tts_engine::Segment| {
                    let span = (segment.start, segment.end);
                    ui.send(UiMessage::Segment {
                        verb: "Génération",
                        index,
                        total,
                        span,
                    });
                };
                engine.synthesize_text(
                    key,
                    &text,
                    &stop,
                    Some(&on_segment),
                    speed,
                    volume,
                    &mut |chunk| {
                        wav.write(&chunk)
                            .map_err(|error| write_error = Some(error))
                            .is_ok()
                    },
                )?;
                if let Some(error) = write_error {
                    return Err(error);
                }
                if stop.load(Ordering::SeqCst) || wav.finish()? == 0 {
                    return Ok(None);
                }
                if export_format != WAV_FORMAT {
                    ui.send(UiMessage::Status(format!(
                        "Conversion en {}…",
                        export_format.extension.to_uppercase()
                    )));
                    let converted = convert_audio(&wav_path, &path, export_format);
                    let _ = fs::remove_file(&wav_path);
                    converted?;
                }
                Ok(Some(path))
            })();
            match result {
                Ok(path) => ui.send(UiMessage::ExportDone(path)),
                Err(error) => ui.send(UiMessage::Error(format!("Export impossible : {error:#}"))),
            }
        });
    }

    fn on_export_done(&mut self, path: Option<PathBuf>) {
        self.finish_task();
        self.status = match path {
            None => "Export annulé.".into(),
            Some(path) => format!("Fichier écrit : {}", path.display()),
        };
    }

    // ------------------------------------------------------------ helpers --

    /// Applique les messages des threads de travail (thread de l'interface).
    fn pump(&mut self) {
        while let Ok(message) = self.ui_receiver.try_recv() {
            match message {
                UiMessage::DownloadProgress(fraction, message) => {
                    self.progress = fraction.clamp(0.0, 1.0);
                    self.status = message;
                }
                UiMessage::DownloadDone { key, then } => self.on_download_done(key, then),
                UiMessage::Segment {
                    verb,
                    index,
                    total,
                    span,
                } => self.show_segment(verb, index, total, span),
                UiMessage::PlayDone(count) => self.on_play_done(count),
                UiMessage::ExportDone(path) => self.on_export_done(path),
                UiMessage::Status(message) => self.status = message,
                UiMessage::Error(message) => self.on_error(&message),
            }
        }
    }

    fn set_busy(&mut self, busy: Option<Busy>) {
        self.busy = busy;
        self.paused = false;
    }

    /// Fin de tâche (normale, interrompue ou en erreur) : interface rendue.
    fn finish_task(&mut self) {
        self.set_busy(None);
        self.highlight = None;
        self.progress = 0.0;
    }

    fn on_error(&mut self, message: &str) {
        self.finish_task();
        self.status = "Erreur.".into();
        show_error(message);
    }
}

impl eframe::App for SouffleurApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        egui::Frame::central_panel(ui.style())
            .inner_margin(12.0)
            .show(ui, |ui| self.build_ui(ui));
    }

    fn on_exit(&mut self) {
        // Les threads de travail voient le drapeau et s'arrêtent ; le
        // processus se termine de toute façon avec la fenêtre.
        self.player.stop();
    }
}

/// Texte mis en forme pour la zone de saisie, segment courant surligné.
fn highlighted_job(text: &str, highlight: Option<(usize, usize)>, color: Color32) -> LayoutJob {
    let normal = TextFormat {
        font_id: FontId::proportional(TEXT_FONT_SIZE),
        color,
        ..TextFormat::default()
    };
    let current = TextFormat {
        background: CURRENT_BACKGROUND,
        color: CURRENT_FOREGROUND,
        ..normal.clone()
    };
    let mut job = LayoutJob::default();
    // Positions vérifiées : le texte peut avoir changé depuis le découpage.
    match highlight.filter(|&(start, end)| start <= end && text.get(start..end).is_some()) {
        Some((start, end)) => {
            job.append(&text[..start], 0.0, normal.clone());
            job.append(&text[start..end], 0.0, current);
            job.append(&text[end..], 0.0, normal);
        }
        None => job.append(text, 0.0, normal),
    }
    job
}

/// Pastille pleine si le modèle est installé, vide sinon. Dessinée plutôt
/// qu'écrite : les polices d'egui n'ont pas les caractères ● et ○.
fn voice_state_dot(ui: &mut egui::Ui, installed: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    let color = ui.visuals().text_color();
    if installed {
        ui.painter().circle_filled(rect.center(), 4.0, color);
    } else {
        ui.painter()
            .circle_stroke(rect.center(), 4.0, egui::Stroke::new(1.0, color));
    }
}

/// Libellé + curseur + valeur affichée, sur une ligne.
fn slider_row(
    ui: &mut egui::Ui,
    text: &str,
    value: &mut f64,
    range: std::ops::RangeInclusive<f64>,
    format: impl Fn(f64) -> String,
) -> egui::Response {
    ui.horizontal(|ui| {
        ui.label(text);
        let shown = format(*value);
        let value_width = 56.0;
        ui.spacing_mut().slider_width =
            (ui.available_width() - value_width - ui.spacing().item_spacing.x).max(60.0);
        let response = ui.add(egui::Slider::new(value, range).show_value(false));
        ui.add_sized(
            [value_width, ui.spacing().interact_size.y],
            egui::Label::new(shown),
        );
        response
    })
    .inner
}

/// Fin d'un réglage : relâchement de la souris, clic simple ou flèches du clavier.
fn released(response: &egui::Response) -> bool {
    response.drag_stopped() || (response.changed() && !response.dragged())
}

/// Lit un fichier texte : UTF-8 (BOM toléré), sinon latin-1, qui accepte
/// n'importe quel octet — un fichier n'est donc jamais refusé pour son
/// encodage, au pire quelques accents sont faux.
///
/// Les fins de ligne Windows et Mac classiques sont normalisées.
fn read_text_file(path: &Path) -> std::io::Result<String> {
    let raw = fs::read(path)?;
    let raw = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&raw);
    let content = match std::str::from_utf8(raw) {
        Ok(content) => content.to_owned(),
        Err(_) => raw.iter().map(|&byte| char::from(byte)).collect(),
    };
    Ok(content.replace("\r\n", "\n").replace('\r', "\n"))
}

/// Arrondit au pas le plus proche, dans les bornes (et sans bruit flottant).
fn snap(value: f64, step: f64, low: f64, high: f64) -> f64 {
    let snapped = (value.clamp(low, high) / step).round() * step;
    (snapped * 10_000.0).round() / 10_000.0
}

/// `chemin` + suffixe (`a.mp3` -> `a.mp3.tmp.wav`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn spawn(name: &str, work: impl FnOnce() + Send + 'static) {
    thread::Builder::new()
        .name(name.into())
        .spawn(work)
        .expect("création d'un thread de travail");
}

fn show_error(message: &str) {
    MessageDialog::new()
        .set_level(MessageLevel::Error)
        .set_title(APP_TITLE)
        .set_description(message)
        .show();
}

fn main() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(WINDOW_TITLE)
        .with_inner_size([860.0, 640.0])
        .with_min_inner_size([620.0, 460.0]);
    // Best effort : une icône illisible ne doit jamais empêcher le démarrage.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(SouffleurApp::new(cc)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_rounds_to_the_step_within_bounds() {
        assert_eq!(snap(1.26, 0.05, 0.5, 2.0), 1.25);
        assert_eq!(snap(3.0, 0.05, 0.5, 2.0), 2.0);
        assert_eq!(snap(0.123, 0.05, 0.0, 1.0), 0.1);
    }

    #[test]
    fn text_files_fall_back_to_latin1_and_normalize_newlines() {
        let dir = std::env::temp_dir().join(format!("souffleur-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let utf8 = dir.join("utf8.txt");
        fs::write(&utf8, b"\xEF\xBB\xBF\xC3\xA9t\xC3\xA9\r\nfin\r").unwrap();
        assert_eq!(read_text_file(&utf8).unwrap(), "été\nfin\n");
        let latin1 = dir.join("latin1.txt");
        fs::write(&latin1, b"\xE9t\xE9").unwrap();
        assert_eq!(read_text_file(&latin1).unwrap(), "été");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn highlight_outside_the_text_is_ignored() {
        assert_eq!(
            highlighted_job("abc", Some((1, 2)), Color32::WHITE)
                .sections
                .len(),
            3
        );
        assert_eq!(
            highlighted_job("abc", Some((1, 9)), Color32::WHITE)
                .sections
                .len(),
            1
        );
    }
}
