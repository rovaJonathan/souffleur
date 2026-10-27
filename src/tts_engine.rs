//! Logique TTS : catalogue de voix, téléchargement des modèles, synthèse Piper.
//!
//! Ce module ne connaît rien de l'interface : il est utilisable seul (tests).
//!
//! Principes :
//!   * le texte est découpé en segments (paragraphes -> phrases -> groupes courts)
//!     pour que la génération démarre vite et reste fluide sur les longs textes ;
//!   * la synthèse est poussée chunk par chunk vers l'appelant, donc consommable
//!     en streaming par le lecteur audio sans jamais tout garder en mémoire.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;

use crate::piper::{PiperVoice, SynthesisConfig};
use crate::settings::voices_dir;

// --------------------------------------------------------------------------- //
// Catalogue de voix
// --------------------------------------------------------------------------- //

/// Une voix Piper du dépôt Hugging Face `rhasspy/piper-voices`.
pub struct Voice {
    /// ex. "fr_FR-siwis-medium"
    pub key: &'static str,
    /// libellé affiché dans l'interface
    pub label: &'static str,
}

pub const VOICES: &[Voice] = &[
    Voice {
        key: "fr_FR-siwis-medium",
        label: "Français — Siwis (medium)",
    },
    Voice {
        key: "fr_FR-upmc-medium",
        label: "Français — UPMC (medium)",
    },
    Voice {
        key: "en_US-lessac-medium",
        label: "Anglais US — Lessac (medium)",
    },
    Voice {
        key: "en_US-amy-medium",
        label: "Anglais US — Amy (medium)",
    },
    Voice {
        key: "en_GB-alba-medium",
        label: "Anglais GB — Alba (medium)",
    },
];

pub const DEFAULT_VOICE: &str = "fr_FR-siwis-medium";

pub fn voice_by_key(key: &str) -> Option<&'static Voice> {
    VOICES.iter().find(|voice| voice.key == key)
}

/// Retourne (modèle .onnx, config .onnx.json) pour une voix donnée.
pub fn voice_paths(key: &str) -> (PathBuf, PathBuf) {
    let base = voices_dir();
    (
        base.join(format!("{key}.onnx")),
        base.join(format!("{key}.onnx.json")),
    )
}

/// Vrai si le modèle est déjà téléchargé localement.
pub fn is_voice_available(key: &str) -> bool {
    let (model, config) = voice_paths(key);
    model
        .metadata()
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
        && config.is_file()
}

/// URLs Hugging Face (modèle, config), déduites de la clé :
/// fr_FR-siwis-medium -> fr / fr_FR / siwis / medium (arborescence de Piper).
fn voice_urls(key: &str) -> Result<(String, String)> {
    static VOICE_PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = VOICE_PATTERN.get_or_init(|| {
        Regex::new(r"^(?P<family>[^-_]+)_(?P<region>[^-]+)-(?P<name>[^-]+)-(?P<quality>.+)$")
            .unwrap()
    });
    let caps = pattern
        .captures(key)
        .ok_or_else(|| anyhow!("Nom de voix invalide : {key:?}"))?;
    let (family, name, quality) = (&caps["family"], &caps["name"], &caps["quality"]);
    let lang_code = format!("{family}_{}", &caps["region"]);
    let url = |ext: &str| {
        format!(
            "https://huggingface.co/rhasspy/piper-voices/resolve/main/\
             {family}/{lang_code}/{name}/{quality}/{lang_code}-{name}-{quality}{ext}"
        )
    };
    Ok((url(".onnx"), url(".onnx.json")))
}

/// Télécharge modèle + config depuis Hugging Face.
///
/// `progress` est appelé avec (fraction entre 0 et 1, message). Le
/// téléchargement se fait dans un fichier `.part` renommé à la fin, pour ne
/// jamais laisser un modèle tronqué sur le disque.
pub fn download_voice(
    key: &str,
    progress: &dyn Fn(f32, String),
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    let (model_url, config_url) = voice_urls(key)?;
    let (model_path, config_path) = voice_paths(key);

    // La config est minuscule : on la récupère d'abord, sans détail de progression.
    progress(0.0, format!("Téléchargement de la configuration de {key}…"));
    download_file(&config_url, &config_path, cancel, &|_, _| {})?;

    progress(0.0, format!("Téléchargement du modèle {key}…"));
    download_file(&model_url, &model_path, cancel, &|done, total| {
        let fraction = if total > 0 {
            done as f32 / total as f32
        } else {
            0.0
        };
        let mut message = format!("Téléchargement de {key} — {:.1} Mo", done as f64 / 1e6);
        if total > 0 {
            message += &format!(" / {:.1} Mo", total as f64 / 1e6);
        }
        progress(fraction, message);
    })?;
    progress(1.0, format!("Voix {key} prête."));
    Ok(())
}

fn download_file(
    url: &str,
    destination: &Path,
    cancel: Option<&AtomicBool>,
    progress: &dyn Fn(u64, u64),
) -> Result<()> {
    let temp = part_path(destination);
    let result = (|| {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .build()
            .into();
        let response = agent.get(url).header("User-Agent", "souffleur").call()?;
        let total = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok()?.parse().ok())
            .unwrap_or(0);
        let mut reader = response.into_body().into_reader();
        let mut handle = BufWriter::new(File::create(&temp)?);
        let mut block = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        loop {
            if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                bail!("Téléchargement annulé.");
            }
            let read = reader.read(&mut block)?;
            if read == 0 {
                break;
            }
            handle.write_all(&block[..read])?;
            done += read as u64;
            progress(done, total);
        }
        handle.flush()?;
        drop(handle);
        fs::rename(&temp, destination)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}

/// `<nom>.part` à côté du fichier final.
fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

// --------------------------------------------------------------------------- //
// Découpage du texte
// --------------------------------------------------------------------------- //

/// Taille cible d'un segment, en caractères. Assez court pour un premier son
/// rapide, assez long pour garder une prosodie naturelle.
pub const MAX_SEGMENT_CHARS: usize = 350;

/// Intervalle [début, fin) d'octets dans le texte d'origine.
type Span = (usize, usize);

struct Separators {
    /// Paragraphe : au moins une ligne vide.
    paragraph: Regex,
    /// Fin de phrase : ponctuation forte, éventuels guillemets/parenthèses,
    /// puis espace. Le groupe 1 reste rattaché à la phrase.
    sentence_end: Regex,
    /// Ponctuation faible, utilisée seulement pour casser une phrase trop longue.
    soft_break: Regex,
    whitespace: Regex,
}

fn separators() -> &'static Separators {
    static SEPARATORS: OnceLock<Separators> = OnceLock::new();
    SEPARATORS.get_or_init(|| Separators {
        paragraph: Regex::new(r"\n\s*\n").unwrap(),
        sentence_end: Regex::new(r#"([.!?…]["'»)\]]*)\s+"#).unwrap(),
        soft_break: Regex::new(r"([,;:])\s+").unwrap(),
        whitespace: Regex::new(r"\s+").unwrap(),
    })
}

/// Un morceau de texte synthétisé d'un bloc, avec sa position d'origine.
///
/// `text` est la version normalisée (espaces et retours à la ligne réduits à
/// un espace) envoyée à Piper ; `start`/`end` situent le segment (en octets)
/// dans le texte fourni par l'utilisateur, pour le surligner pendant la lecture.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// Découpe le texte en segments synthétisables l'un après l'autre, en
/// conservant la position de chaque segment.
///
/// Le découpage travaille sur des intervalles du texte d'origine plutôt que
/// sur des copies, pour que les positions restent exactes malgré la
/// normalisation des espaces.
pub fn segment_text(text: &str, max_chars: usize) -> Vec<Segment> {
    let mut segments = Vec::new();
    for paragraph in spans(text, (0, text.len()), &separators().paragraph) {
        let mut buffer: Option<Span> = None;
        for sentence in sentence_spans(text, paragraph, max_chars) {
            buffer = match buffer {
                None => Some(sentence),
                Some(current) if normalized_len(text, (current.0, sentence.1)) <= max_chars => {
                    Some((current.0, sentence.1))
                }
                Some(current) => {
                    segments.push(segment(text, current));
                    Some(sentence)
                }
            };
        }
        if let Some(current) = buffer {
            segments.push(segment(text, current));
        }
    }
    segments
}

fn segment(text: &str, span: Span) -> Segment {
    Segment {
        text: normalize(text, span),
        start: span.0,
        end: span.1,
    }
}

/// Espaces et retours à la ligne réduits à un seul espace.
fn normalize(text: &str, span: Span) -> String {
    text[span.0..span.1]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalized_len(text: &str, span: Span) -> usize {
    normalize(text, span).chars().count()
}

/// Sous-intervalles de `span` entre deux occurrences de `separator`.
///
/// Chaque morceau est débarrassé des espaces à ses bords ; les vides sont
/// ignorés. Si le séparateur a un groupe 1, son contenu reste rattaché au
/// morceau précédent (guillemet fermant après le point, par exemple).
fn spans(text: &str, span: Span, separator: &Regex) -> Vec<Span> {
    let (base, end) = span;
    let has_group = separator.captures_len() > 1;
    let mut cursor = base;
    let mut pieces = Vec::new();
    for caps in separator.captures_iter(&text[base..end]) {
        let whole = caps.get(0).unwrap();
        let piece_end = base
            + if has_group {
                caps.get(1).unwrap().end()
            } else {
                whole.start()
            };
        pieces.extend(stripped(text, (cursor, piece_end)));
        cursor = base + whole.end();
    }
    pieces.extend(stripped(text, (cursor, end)));
    pieces
}

fn stripped(text: &str, (start, end): Span) -> Option<Span> {
    let piece = &text[start..end];
    let trimmed_start = piece.trim_start();
    let start = start + (piece.len() - trimmed_start.len());
    let end = start + trimmed_start.trim_end().len();
    (start < end).then_some((start, end))
}

fn sentence_spans(text: &str, paragraph: Span, max_chars: usize) -> Vec<Span> {
    let mut sentences = Vec::new();
    for sentence in spans(text, paragraph, &separators().sentence_end) {
        if normalized_len(text, sentence) <= max_chars {
            sentences.push(sentence);
        } else {
            sentences.extend(wrap_long_sentence(text, sentence, max_chars));
        }
    }
    sentences
}

/// Phrase sans ponctuation forte : on casse sur virgules, sinon sur les mots.
fn wrap_long_sentence(text: &str, sentence: Span, max_chars: usize) -> Vec<Span> {
    let mut pieces = Vec::new();
    for piece in spans(text, sentence, &separators().soft_break) {
        if normalized_len(text, piece) <= max_chars {
            pieces.push(piece);
            continue;
        }
        let mut buffer: Option<Span> = None;
        for word in spans(text, piece, &separators().whitespace) {
            buffer = match buffer {
                None => Some(word),
                Some(current) if normalized_len(text, (current.0, word.1)) <= max_chars => {
                    Some((current.0, word.1))
                }
                Some(current) => {
                    pieces.push(current);
                    Some(word)
                }
            };
        }
        pieces.extend(buffer);
    }
    pieces
}

// --------------------------------------------------------------------------- //
// Moteur de synthèse
// --------------------------------------------------------------------------- //

/// Silence inséré entre deux segments, pour éviter un enchaînement abrupt.
const SEGMENT_GAP_MS: u32 = 120;

/// Audio PCM mono 16 bits, avec sa fréquence d'échantillonnage.
#[derive(Clone)]
pub struct AudioChunk {
    pub sample_rate: u32,
    pub samples: Vec<i16>,
}

/// Vitesse de lecture, en multiple de la cadence naturelle de la voix.
pub const SPEED_MIN: f64 = 0.5;
pub const SPEED_MAX: f64 = 2.0;
pub const DEFAULT_SPEED: f64 = 1.0;

/// Volume entre 0 et 1. Piper normalise l'audio avant d'appliquer ce facteur,
/// puis écrête à ±1 : monter au-dessus de 1 ne ferait que saturer.
pub const VOLUME_MIN: f64 = 0.0;
pub const VOLUME_MAX: f64 = 1.0;
pub const DEFAULT_VOLUME: f64 = 1.0;

/// Traduit vitesse/volume en réglages Piper pour une voix donnée.
///
/// Piper raisonne en `length_scale`, une *durée* : plus elle est grande, plus
/// la voix est lente — d'où l'inverse. Le facteur est appliqué relativement au
/// réglage du modèle, qui vaut souvent 1 mais n'y est pas tenu.
pub fn synthesis_config(voice: &PiperVoice, speed: f64, volume: f64) -> SynthesisConfig {
    SynthesisConfig {
        length_scale: Some(voice.config.length_scale / speed.clamp(SPEED_MIN, SPEED_MAX) as f32),
        volume: volume.clamp(VOLUME_MIN, VOLUME_MAX) as f32,
    }
}

/// Appelé avec (index, total, segment) avant la synthèse de chaque segment.
pub type SegmentCallback<'a> = &'a dyn Fn(usize, usize, &Segment);

/// Charge les voix Piper (avec cache) et produit de l'audio PCM 16 bits.
#[derive(Default)]
pub struct PiperEngine {
    voices: Mutex<HashMap<String, Arc<PiperVoice>>>,
}

impl PiperEngine {
    /// Charge la voix, ou la retourne depuis le cache (chargement ~1 s).
    ///
    /// Le verrou est gardé pendant le chargement : un second appel pour la
    /// même voix attend la fin du premier au lieu de la charger deux fois.
    pub fn load(&self, key: &str) -> Result<Arc<PiperVoice>> {
        let mut voices = self
            .voices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(voice) = voices.get(key) {
            return Ok(voice.clone());
        }
        if !is_voice_available(key) {
            bail!("Modèle {key} absent : téléchargez-le avant de lancer la synthèse.");
        }
        let (model_path, config_path) = voice_paths(key);
        let voice = Arc::new(PiperVoice::load(&model_path, &config_path)?);
        voices.insert(key.to_string(), voice.clone());
        Ok(voice)
    }

    /// Génère l'audio des segments en streaming : chaque chunk est passé à
    /// `emit` avec l'index du segment dont il provient.
    ///
    /// Cet index permet à l'interface de suivre ce qui est *joué* et non ce qui
    /// est *généré* : avec le préchargement, la synthèse a plusieurs secondes
    /// d'avance sur la lecture. `emit` retourne `false` pour tout arrêter
    /// (consommateur parti).
    ///
    /// `on_segment(index, total, segment)` est appelé avant la synthèse de
    /// chaque segment. `stop` est vérifié entre chaque chunk : l'arrêt est
    /// quasi immédiat.
    ///
    /// `speed` est relue avant chaque segment, ce qui permet de changer la
    /// vitesse en cours de lecture. Le segment déjà en synthèse garde son
    /// ancienne cadence.
    #[allow(clippy::too_many_arguments)]
    pub fn synthesize_segments(
        &self,
        key: &str,
        segments: &[Segment],
        stop: &AtomicBool,
        on_segment: Option<SegmentCallback>,
        speed: &dyn Fn() -> f64,
        volume: f64,
        emit: &mut dyn FnMut(usize, AudioChunk) -> bool,
    ) -> Result<()> {
        let voice = self.load(key)?;
        let total = segments.len();
        let mut current: Option<(f64, SynthesisConfig)> = None;
        let mut sample_rate: Option<u32> = None;

        for (index, segment) in segments.iter().enumerate() {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if let Some(on_segment) = on_segment {
                on_segment(index, total, segment);
            }

            let wanted_speed = speed();
            let syn_config = match current {
                Some((current_speed, config)) if current_speed == wanted_speed => config,
                _ => {
                    let config = synthesis_config(&voice, wanted_speed, volume);
                    current = Some((wanted_speed, config));
                    config
                }
            };

            // Silence de liaison entre deux segments (pas avant le premier).
            if let Some(rate) = sample_rate.filter(|_| index > 0)
                && !emit(index, silence(rate, SEGMENT_GAP_MS))
            {
                return Ok(());
            }

            // Piper peut rendre plusieurs chunks par segment (un par phrase).
            for samples in voice.synthesize(&segment.text, syn_config)? {
                let samples = samples?;
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let rate = voice.config.sample_rate;
                sample_rate = Some(rate);
                if !emit(
                    index,
                    AudioChunk {
                        sample_rate: rate,
                        samples,
                    },
                ) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Génère l'audio de tout le texte, segment par segment, en streaming.
    ///
    /// Raccourci : découpe le texte puis délègue à `synthesize_segments`, sans
    /// l'index de segment (suffisant pour l'export).
    #[allow(clippy::too_many_arguments)]
    pub fn synthesize_text(
        &self,
        key: &str,
        text: &str,
        stop: &AtomicBool,
        on_segment: Option<SegmentCallback>,
        speed: f64,
        volume: f64,
        emit: &mut dyn FnMut(AudioChunk) -> bool,
    ) -> Result<()> {
        let segments = segment_text(text, MAX_SEGMENT_CHARS);
        self.synthesize_segments(
            key,
            &segments,
            stop,
            on_segment,
            &|| speed,
            volume,
            &mut |_index, chunk| emit(chunk),
        )
    }
}

fn silence(sample_rate: u32, milliseconds: u32) -> AudioChunk {
    let length = (u64::from(sample_rate) * u64::from(milliseconds) / 1000) as usize;
    AudioChunk {
        sample_rate,
        samples: vec![0; length],
    }
}

/// Écrit un WAV mono 16 bits (format natif de Piper) chunk par chunk.
///
/// Rien n'est gardé en mémoire : une heure d'audio s'écrit avec la même
/// empreinte qu'une phrase. Le fichier est d'abord écrit sous `<nom>.part`
/// puis renommé par `finish()` : un export interrompu (arrêt, erreur, abandon
/// du writer) ne laisse jamais un WAV tronqué sous le nom final, le `.part`
/// étant supprimé. Si aucun chunk n'a été écrit, aucun fichier n'est créé.
pub struct WavWriter {
    path: PathBuf,
    part_path: PathBuf,
    /// Échantillons écrits, 0 si rien.
    pub frames: u64,
    // Ouvert au premier chunk : le WAV a besoin de la fréquence en tête.
    wav: Option<hound::WavWriter<BufWriter<File>>>,
}

impl WavWriter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let part_path = part_path(&path);
        Self {
            path,
            part_path,
            frames: 0,
            wav: None,
        }
    }

    pub fn write(&mut self, chunk: &AudioChunk) -> Result<()> {
        let wav = match &mut self.wav {
            Some(wav) => wav,
            empty => {
                let spec = hound::WavSpec {
                    channels: 1,
                    sample_rate: chunk.sample_rate,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                };
                empty.insert(hound::WavWriter::create(&self.part_path, spec)?)
            }
        };
        let mut writer = wav.get_i16_writer(chunk.samples.len() as u32);
        for &sample in &chunk.samples {
            writer.write_sample(sample);
        }
        writer.flush()?;
        self.frames += chunk.samples.len() as u64;
        Ok(())
    }

    /// Termine l'écriture et renomme le fichier (atomique, écrase un ancien
    /// fichier). Retourne le nombre d'échantillons écrits.
    pub fn finish(mut self) -> Result<u64> {
        let Some(wav) = self.wav.take() else {
            return Ok(0);
        };
        wav.finalize()?;
        fs::rename(&self.part_path, &self.path)?;
        Ok(self.frames)
    }
}

impl Drop for WavWriter {
    /// Abandon (arrêt demandé, erreur) : le fichier partiel est supprimé.
    fn drop(&mut self) {
        if self.wav.take().is_some() {
            let _ = fs::remove_file(&self.part_path);
        }
    }
}

// --------------------------------------------------------------------------- //
// Formats compressés (via ffmpeg, optionnel)
// --------------------------------------------------------------------------- //

#[derive(Debug, PartialEq)]
pub struct ExportFormat {
    pub label: &'static str,
    /// sans le point
    pub extension: &'static str,
    /// vide pour le WAV : aucune conversion
    pub ffmpeg_args: &'static [&'static str],
}

/// Le WAV vient en premier : format par défaut, toujours disponible. Les autres
/// exigent `ffmpeg` (qualité VBR proche du transparent pour de la voix mono).
pub const EXPORT_FORMATS: &[ExportFormat] = &[
    ExportFormat {
        label: "Fichier WAV",
        extension: "wav",
        ffmpeg_args: &[],
    },
    ExportFormat {
        label: "Fichier MP3",
        extension: "mp3",
        ffmpeg_args: &["-codec:a", "libmp3lame", "-q:a", "2"],
    },
    ExportFormat {
        label: "Fichier OGG (Vorbis)",
        extension: "ogg",
        ffmpeg_args: &["-codec:a", "libvorbis", "-q:a", "5"],
    },
];

pub const WAV_FORMAT: &ExportFormat = &EXPORT_FORMATS[0];

/// Chemin de `ffmpeg` s'il est installé, sinon `None`.
///
/// Outre le `PATH`, regarde les préfixes Homebrew : une application lancée
/// depuis le Finder n'hérite que d'un `PATH` minimal.
pub fn ffmpeg_path() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if cfg!(target_os = "macos") {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    }
    dirs.into_iter()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// WAV seul sans ffmpeg, tous les formats avec.
pub fn available_export_formats() -> &'static [ExportFormat] {
    if ffmpeg_path().is_none() {
        std::slice::from_ref(WAV_FORMAT)
    } else {
        EXPORT_FORMATS
    }
}

/// Format déduit de l'extension du fichier choisi ; WAV si inconnue.
pub fn export_format_for(path: &Path) -> &'static ExportFormat {
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    available_export_formats()
        .iter()
        .find(|format| format.extension == extension)
        .unwrap_or(WAV_FORMAT)
}

/// Convertit un WAV avec ffmpeg ; écrit via `.part` puis renomme.
///
/// Les dernières lignes d'erreur de `ffmpeg` remontent dans l'erreur, pour
/// qu'un encodeur manquant (`libmp3lame` absent de la compilation) soit
/// compréhensible.
pub fn convert_audio(
    wav_path: &Path,
    output_path: &Path,
    export_format: &ExportFormat,
) -> Result<()> {
    let ffmpeg = ffmpeg_path().context("ffmpeg est introuvable dans le PATH.")?;
    let part = part_path(output_path);
    let mut command = Command::new(ffmpeg);
    command
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(wav_path)
        .args(export_format.ffmpeg_args)
        // l'extension .part ne dit rien à ffmpeg
        .args(["-f", export_format.extension])
        .arg(&part)
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        // Pas de console qui clignote derrière l'application fenêtrée.
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let result = (|| {
        let completed = command.output().context("Lancement de ffmpeg")?;
        if !completed.status.success() {
            let stderr = String::from_utf8_lossy(&completed.stderr);
            let lines: Vec<&str> = stderr.trim().lines().collect();
            let detail = lines[lines.len().saturating_sub(3)..].join(" / ");
            bail!(
                "ffmpeg a échoué : {}",
                if detail.is_empty() {
                    "sans message"
                } else {
                    &detail
                }
            );
        }
        fs::rename(&part, output_path)?;
        Ok(())
    })();
    let _ = fs::remove_file(&part);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(text: &str, max_chars: usize) -> Vec<String> {
        segment_text(text, max_chars)
            .into_iter()
            .map(|s| s.text)
            .collect()
    }

    #[test]
    fn paragraphs_and_sentences_are_grouped_up_to_the_limit() {
        let text = "Bonjour.  Comment\nça va ?\n\nNouveau paragraphe !";
        assert_eq!(
            texts(text, 350),
            ["Bonjour. Comment ça va ?", "Nouveau paragraphe !"]
        );
        // Phrases plus longues que la limite : coupe de secours sur les mots.
        assert_eq!(
            texts(text, 10),
            [
                "Bonjour.",
                "Comment ça",
                "va ?",
                "Nouveau",
                "paragraphe",
                "!"
            ]
        );
    }

    #[test]
    fn spans_point_into_the_original_text() {
        let text = "  «Été.» Puis\tlà.\n";
        for segment in segment_text(text, 5) {
            assert_eq!(normalize(text, (segment.start, segment.end)), segment.text);
        }
        assert_eq!(texts(text, 5), ["«Été.»", "Puis", "là."]);
    }

    #[test]
    fn long_sentences_break_on_commas_then_words() {
        assert_eq!(
            texts("un deux, trois quatre cinq", 12),
            ["un deux,", "trois quatre", "cinq"]
        );
    }

    #[test]
    fn voice_urls_follow_the_hugging_face_layout() {
        let (model, config) = voice_urls("fr_FR-siwis-medium").unwrap();
        assert_eq!(
            model,
            "https://huggingface.co/rhasspy/piper-voices/resolve/main/fr/fr_FR/siwis/medium/fr_FR-siwis-medium.onnx"
        );
        assert!(config.ends_with("fr_FR-siwis-medium.onnx.json"));
        assert!(voice_urls("invalide").is_err());
    }
}
