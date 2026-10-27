//! Chemins applicatifs et préférences persistées entre deux sessions.
//!
//! Les réglages sont stockés dans un simple fichier JSON, dans le répertoire de
//! données standard de la plateforme. Même emplacement et même format que la
//! version Python : préférences et voix déjà téléchargées sont reprises.

use std::env;
use std::fs;
use std::path::PathBuf;

use serde_json::{Map, Value};

pub const APP_NAME: &str = "souffleur";
/// Ancien nom du répertoire de données, migré au premier lancement.
const LEGACY_APP_NAME: &str = "piper-tts-gui";
const SETTINGS_FILE: &str = "settings.json";

fn home() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Emplacement standard des données pour cette plateforme (pas de création).
fn platform_dir(app_name: &str) -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join(app_name);
    }
    if cfg!(target_os = "macos") {
        return home()
            .join("Library")
            .join("Application Support")
            .join(app_name);
    }
    if cfg!(windows) {
        return env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home)
            .join(app_name);
    }
    home().join(".local").join("share").join(app_name)
}

/// Répertoire de données de l'application, créé si nécessaire.
pub fn app_data_dir() -> PathBuf {
    let base = platform_dir(APP_NAME);
    if !base.exists() {
        migrate_legacy(&base);
    }
    let _ = fs::create_dir_all(&base);
    base
}

/// Reprend les données de l'ancien nom : évite de retélécharger les voix.
fn migrate_legacy(base: &PathBuf) {
    let legacy = platform_dir(LEGACY_APP_NAME);
    if !legacy.is_dir() {
        return;
    }
    // Migration impossible : l'appli repart simplement sur un dossier vide.
    if let Some(parent) = base.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::rename(&legacy, base);
}

/// Répertoire où sont téléchargés les modèles de voix Piper.
pub fn voices_dir() -> PathBuf {
    let path = app_data_dir().join("voices");
    let _ = fs::create_dir_all(&path);
    path
}

/// Petit conteneur clé/valeur persisté en JSON.
#[derive(Default)]
pub struct Settings {
    data: Map<String, Value>,
}

impl Settings {
    /// Fichier absent ou corrompu : on repart sur des réglages vides.
    pub fn load() -> Self {
        let data = fs::read(app_data_dir().join(SETTINGS_FILE))
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default();
        Self { data }
    }

    /// Une préférence non sauvegardée ne doit jamais casser l'appli.
    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.data) {
            let _ = fs::write(app_data_dir().join(SETTINGS_FILE), json);
        }
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(Value::as_str)
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        // Nombre, ou texte numérique si le fichier a été édité à la main.
        let value = self.data.get(key)?;
        value
            .as_f64()
            .or_else(|| value.as_str()?.trim().parse().ok())
    }

    /// Enregistre immédiatement : pas de perte si l'appli est tuée.
    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.data.insert(key.to_string(), value.into());
        self.save();
    }
}
