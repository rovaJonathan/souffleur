//! Phonémisation avec espeak-ng (FFI vers la bibliothèque du système).
//!
//! espeak-ng est un état global, non réentrant : tout passe par un verrou
//! unique, comme dans le paquet Python de Piper.
//!
//! Piper s'appuie sur `espeak_TextToPhonemesWithTerminator`, une fonction de
//! son propre fork d'espeak-ng absente des versions distribuées (Homebrew,
//! Debian). On utilise donc `espeak_TextToPhonemes`, qui rend les phonèmes
//! proposition par proposition et avance le pointeur de texte : la
//! ponctuation de fin se lit dans le morceau de texte consommé.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::{Mutex, OnceLock};

const AUDIO_OUTPUT_SYNCHRONOUS: c_int = 2;
const ESPEAK_INITIALIZE_DONT_EXIT: c_int = 0x8000;
const ESPEAK_CHARS_AUTO: c_int = 0;
const ESPEAK_PHONEMES_IPA: c_int = 0x02;

unsafe extern "C" {
    fn espeak_Initialize(
        output: c_int,
        buflength: c_int,
        path: *const c_char,
        options: c_int,
    ) -> c_int;
    fn espeak_SetVoiceByName(name: *const c_char) -> c_int;
    fn espeak_TextToPhonemes(
        textptr: *mut *const c_void,
        textmode: c_int,
        phonememode: c_int,
    ) -> *const c_char;
}

/// Une proposition phonémisée : phonèmes IPA, ponctuation de fin (vide si
/// aucune), et vrai si elle termine une phrase.
pub struct Clause {
    pub phonemes: String,
    pub terminator: &'static str,
    pub end_of_sentence: bool,
}

/// `Ok(())` une fois espeak-ng initialisé ; l'erreur est gardée pour être
/// resignalée à chaque appel (données introuvables, par exemple).
static ESPEAK: OnceLock<Result<Mutex<()>, String>> = OnceLock::new();

fn lock() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    let state = ESPEAK.get_or_init(|| {
        // Chemin nul : espeak-ng cherche ses données via ESPEAK_DATA_PATH, puis
        // à l'emplacement fixé à sa compilation.
        let rate = unsafe {
            espeak_Initialize(
                AUDIO_OUTPUT_SYNCHRONOUS,
                0,
                ptr::null(),
                ESPEAK_INITIALIZE_DONT_EXIT,
            )
        };
        if rate < 0 {
            Err("espeak-ng n'a pas pu s'initialiser (données espeak-ng-data introuvables ?)".into())
        } else {
            Ok(Mutex::new(()))
        }
    });
    match state {
        Ok(mutex) => Ok(mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())),
        Err(message) => Err(message.clone()),
    }
}

/// Texte -> propositions phonémisées, avec la voix espeak-ng `voice`.
pub fn phonemize(voice: &str, text: &str) -> Result<Vec<Clause>, String> {
    let _guard = lock()?;
    let voice_c = CString::new(voice).map_err(|_| format!("Voix espeak invalide : {voice:?}"))?;
    if unsafe { espeak_SetVoiceByName(voice_c.as_ptr()) } != 0 {
        return Err(format!("Voix espeak-ng introuvable : {voice}"));
    }

    // Un octet nul arrêterait espeak au milieu du texte.
    let text_c = CString::new(text.replace('\0', " ")).expect("octets nuls retirés");
    let bytes = text_c.as_bytes();
    let base = text_c.as_ptr();
    let mut cursor: *const c_void = base.cast();
    let mut clauses = Vec::new();

    while !cursor.is_null() {
        let start = cursor as usize - base as usize;
        let phonemes =
            unsafe { espeak_TextToPhonemes(&mut cursor, ESPEAK_CHARS_AUTO, ESPEAK_PHONEMES_IPA) };
        let consumed = if cursor.is_null() {
            String::from_utf8_lossy(&bytes[start..])
        } else {
            String::from_utf8_lossy(&bytes[start..cursor as usize - base as usize])
        };
        // Sauf en fin de texte, espeak a lu un caractère d'avance : le premier
        // de la proposition suivante, qu'il garde en réserve pour l'appel d'après.
        let mut consumed = consumed.chars();
        if !cursor.is_null() {
            consumed.next_back();
        }
        let phonemes = if phonemes.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(phonemes) }
                .to_string_lossy()
                .into_owned()
        };
        let (terminator, end_of_sentence) = clause_terminator(consumed.as_str());
        clauses.push(Clause {
            phonemes,
            terminator,
            end_of_sentence,
        });
    }
    Ok(clauses)
}

/// Ponctuation qui clôt une proposition, déduite du texte qu'elle a consommé
/// (sans le caractère lu d'avance).
///
/// Mêmes catégories que le pont C de Piper : `.`, `?`, `!` terminent une
/// phrase ; `,`, `:`, `;` non. Les points de suspension (`…` ou `...`) ne
/// donnent aucune ponctuation et ne coupent pas la phrase. Guillemets et
/// parenthèses fermants sont ignorés.
fn clause_terminator(consumed: &str) -> (&'static str, bool) {
    let mut significant = consumed
        .chars()
        .rev()
        .filter(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '»' | '”' | '’' | ')' | ']'));
    match (significant.next(), significant.next()) {
        (Some('.'), Some('.')) => ("", false),
        (Some('.'), _) => (".", true),
        (Some('?'), _) => ("?", true),
        (Some('!'), _) => ("!", true),
        (Some(','), _) => (",", false),
        (Some(':'), _) => (":", false),
        (Some(';'), _) => (";", false),
        _ => ("", false),
    }
}
