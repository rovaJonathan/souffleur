//! Synthèse Piper : phonémisation espeak-ng puis inférence ONNX (modèle VITS).
//!
//! Reprend le pipeline du paquet Python `piper-tts` pour les voix de type
//! « espeak » (toutes celles du catalogue) : texte -> phonèmes par phrase ->
//! identifiants -> audio, normalisé phrase par phrase.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use ort::session::Session;
use ort::value::Tensor;
use regex::Regex;
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use crate::espeak;

const BOS: &str = "^";
const EOS: &str = "$";
const PAD: &str = "_";
const MAX_WAV_VALUE: f32 = 32767.0;

/// Configuration d'une voix (fichier `.onnx.json`).
pub struct PiperConfig {
    pub sample_rate: u32,
    pub espeak_voice: String,
    pub phoneme_id_map: HashMap<String, Vec<i64>>,
    pub num_speakers: u32,
    pub default_speaker_id: i64,
    pub length_scale: f32,
    pub noise_scale: f32,
    pub noise_w_scale: f32,
    /// Suites de voyelles fusionnées en un seul « phonème » (ex. `a` + `ɪ`).
    pub vowel_clusters: HashSet<Vec<String>>,
}

impl PiperConfig {
    fn from_json(json: &Value) -> Result<Self> {
        let number = |pointer: &str, default: f64| {
            json.pointer(pointer)
                .and_then(Value::as_f64)
                .unwrap_or(default)
        };
        let phoneme_type = json
            .get("phoneme_type")
            .and_then(Value::as_str)
            .unwrap_or("espeak");
        if phoneme_type != "espeak" {
            bail!("Type de phonèmes non pris en charge : {phoneme_type}");
        }
        let phoneme_id_map = json
            .get("phoneme_id_map")
            .and_then(Value::as_object)
            .context("phoneme_id_map absent de la configuration")?
            .iter()
            .map(|(phoneme, ids)| {
                let ids = ids
                    .as_array()
                    .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
                    .unwrap_or_default();
                (phoneme.clone(), ids)
            })
            .collect();
        let vowel_clusters = json
            .get("vowel_clusters")
            .and_then(Value::as_array)
            .map(|clusters| {
                clusters
                    .iter()
                    .filter_map(Value::as_array)
                    .map(|cluster| {
                        cluster
                            .iter()
                            .filter_map(|p| p.as_str().map(String::from))
                            .collect()
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            sample_rate: json
                .pointer("/audio/sample_rate")
                .and_then(Value::as_u64)
                .context("audio.sample_rate absent de la configuration")?
                as u32,
            espeak_voice: json
                .pointer("/espeak/voice")
                .and_then(Value::as_str)
                .context("espeak.voice absent de la configuration")?
                .to_string(),
            phoneme_id_map,
            num_speakers: json
                .get("num_speakers")
                .and_then(Value::as_u64)
                .unwrap_or(1) as u32,
            default_speaker_id: json
                .get("default_speaker_id")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            length_scale: number("/inference/length_scale", 1.0) as f32,
            noise_scale: number("/inference/noise_scale", 0.667) as f32,
            noise_w_scale: number("/inference/noise_w", 0.8) as f32,
            vowel_clusters,
        })
    }
}

/// Réglages d'une synthèse : `None` = valeur du modèle.
#[derive(Clone, Copy, Default)]
pub struct SynthesisConfig {
    pub length_scale: Option<f32>,
    /// Multiplicateur appliqué après normalisation, puis écrêtage à ±1.
    pub volume: f32,
}

/// Une voix chargée. La session ONNX demande un accès exclusif pour `run`,
/// d'où le verrou : une même voix ne synthétise qu'une phrase à la fois.
pub struct PiperVoice {
    pub config: PiperConfig,
    session: Mutex<Session>,
}

impl PiperVoice {
    pub fn load(model_path: &Path, config_path: &Path) -> Result<Self> {
        let json: Value = serde_json::from_slice(
            &std::fs::read(config_path)
                .with_context(|| format!("Lecture de {}", config_path.display()))?,
        )
        .with_context(|| format!("Configuration illisible : {}", config_path.display()))?;
        let config = PiperConfig::from_json(&json)?;
        let session = Session::builder()
            .and_then(|mut builder| builder.commit_from_file(model_path))
            .map_err(|e| anyhow!("Chargement du modèle {} : {e}", model_path.display()))?;
        Ok(Self {
            config,
            session: Mutex::new(session),
        })
    }

    /// Texte -> phonèmes, regroupés par phrase.
    pub fn phonemize(&self, text: &str) -> Result<Vec<Vec<String>>> {
        static LANG_SWITCH: OnceLock<Regex> = OnceLock::new();
        // Marqueurs `(en)`… autour des mots d'une autre langue que la voix.
        let lang_switch = LANG_SWITCH.get_or_init(|| Regex::new(r"\([^)]+\)").unwrap());

        let mut sentences = Vec::new();
        let mut sentence: Vec<String> = Vec::new();
        for clause in espeak::phonemize(&self.config.espeak_voice, text).map_err(|e| anyhow!(e))? {
            let mut phonemes = lang_switch.replace_all(&clause.phonemes, "").into_owned();
            // La ponctuation est gardée : elle guide la prosodie du modèle.
            phonemes.push_str(clause.terminator);
            if matches!(clause.terminator, "," | ":" | ";") {
                phonemes.push(' ');
            }
            // Décomposition NFD : les diacritiques deviennent des phonèmes à part.
            sentence.extend(phonemes.nfd().map(String::from));
            if clause.end_of_sentence {
                sentences.push(self.merge_vowel_clusters(std::mem::take(&mut sentence)));
            }
        }
        if !sentence.is_empty() {
            // Texte sans ponctuation finale.
            sentences.push(self.merge_vowel_clusters(sentence));
        }
        Ok(sentences)
    }

    fn merge_vowel_clusters(&self, phonemes: Vec<String>) -> Vec<String> {
        let clusters = &self.config.vowel_clusters;
        let Some(max_len) = clusters.iter().map(Vec::len).max() else {
            return phonemes;
        };
        let mut merged = Vec::with_capacity(phonemes.len());
        let mut i = 0;
        while i < phonemes.len() {
            let found = (2..=max_len.min(phonemes.len() - i))
                .rev()
                .find(|&n| clusters.contains(&phonemes[i..i + n]));
            match found {
                Some(n) => {
                    merged.push(phonemes[i..i + n].concat());
                    i += n;
                }
                None => {
                    merged.push(phonemes[i].clone());
                    i += 1;
                }
            }
        }
        merged
    }

    /// Phonèmes -> identifiants, encadrés de BOS/EOS et entrecoupés de PAD.
    /// Un phonème absent de la table est ignoré, comme dans Piper.
    pub fn phonemes_to_ids(&self, phonemes: &[String]) -> Vec<i64> {
        let map = &self.config.phoneme_id_map;
        let ids_of = |symbol: &str| map.get(symbol).map(Vec::as_slice).unwrap_or_default();
        let mut ids = Vec::new();
        ids.extend_from_slice(ids_of(BOS));
        ids.extend_from_slice(ids_of(PAD));
        for phoneme in phonemes {
            if let Some(phoneme_ids) = map.get(phoneme) {
                ids.extend_from_slice(phoneme_ids);
                ids.extend_from_slice(ids_of(PAD));
            }
        }
        ids.extend_from_slice(ids_of(EOS));
        ids
    }

    /// Synthétise le texte, une phrase à la fois : chaque élément de
    /// l'itérateur est l'audio PCM 16 bits d'une phrase. La phonémisation est
    /// faite d'emblée, l'inférence au fil de l'itération.
    pub fn synthesize<'a>(
        &'a self,
        text: &str,
        syn_config: SynthesisConfig,
    ) -> Result<impl Iterator<Item = Result<Vec<i16>>> + 'a> {
        let sentences = self.phonemize(text)?;
        Ok(sentences
            .into_iter()
            .filter(|phonemes| !phonemes.is_empty())
            .map(move |phonemes| {
                let audio =
                    self.phoneme_ids_to_audio(&self.phonemes_to_ids(&phonemes), syn_config)?;
                Ok(to_int16(normalize(audio), syn_config.volume))
            }))
    }

    /// Inférence brute : audio flottant non normalisé.
    fn phoneme_ids_to_audio(
        &self,
        phoneme_ids: &[i64],
        syn_config: SynthesisConfig,
    ) -> Result<Vec<f32>> {
        let config = &self.config;
        let length = phoneme_ids.len();
        let scales = vec![
            config.noise_scale,
            syn_config.length_scale.unwrap_or(config.length_scale),
            config.noise_w_scale,
        ];
        let tensor_error = |e: ort::Error| anyhow!("Préparation de l'inférence : {e}");
        let input =
            Tensor::from_array(([1usize, length], phoneme_ids.to_vec())).map_err(tensor_error)?;
        let input_lengths =
            Tensor::from_array(([1usize], vec![length as i64])).map_err(tensor_error)?;
        let scales = Tensor::from_array(([3usize], scales)).map_err(tensor_error)?;

        let mut session = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let outputs = if config.num_speakers > 1 {
            let sid = Tensor::from_array(([1usize], vec![config.default_speaker_id])).map_err(tensor_error)?;
            session.run(ort::inputs!["input" => input, "input_lengths" => input_lengths, "scales" => scales, "sid" => sid])
        } else {
            session.run(ort::inputs!["input" => input, "input_lengths" => input_lengths, "scales" => scales])
        }
        .map_err(|e| anyhow!("Inférence ONNX : {e}"))?;
        let (_shape, audio) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow!("Sortie du modèle inattendue : {e}"))?;
        Ok(audio.to_vec())
    }
}

/// Ramène le pic à 1 (silence laissé tel quel).
fn normalize(mut audio: Vec<f32>) -> Vec<f32> {
    let peak = audio
        .iter()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    if peak < 1e-8 {
        audio.fill(0.0);
    } else {
        for sample in &mut audio {
            *sample /= peak;
        }
    }
    audio
}

fn to_int16(audio: Vec<f32>, volume: f32) -> Vec<i16> {
    audio
        .into_iter()
        .map(|sample| ((sample * volume).clamp(-1.0, 1.0) * MAX_WAV_VALUE) as i16)
        .collect()
}
