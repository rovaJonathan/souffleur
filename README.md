# Souffleur

<img src="assets/logo.svg" width="96" alt="Logo de Souffleur" align="right">

Comme au théâtre : il lit votre texte à voix haute, et rien ne sort de la salle.

Petite application de bureau (Rust, [egui](https://github.com/emilk/egui)) pour
faire parler du texte **en local**, avec les voix [Piper](https://github.com/rhasspy/piper).
Aucune donnée ne quitte la machine : seul le premier téléchargement d'un modèle
de voix utilise le réseau.

## Installation

Il faut [Rust](https://rustup.rs) et la bibliothèque espeak-ng (phonémisation) :

```bash
brew install espeak-ng                          # macOS
sudo apt install libespeak-ng-dev libasound2-dev  # Debian / Ubuntu
cargo run --release
```

ONNX Runtime (l'inférence des modèles) est téléchargé et lié statiquement à la
compilation par la crate `ort` : rien d'autre à installer.

Sous macOS, pour une icône dans le Dock et Launchpad :

```bash
cargo xtask macos-app
```

Cela compile en mode optimisé et installe `~/Applications/Souffleur.app`. Le
.app contient le binaire : il ne dépend plus de l'emplacement du dépôt, seulement
de l'espeak-ng de Homebrew.

Optionnel : avec `ffmpeg` dans le `PATH` (`brew install ffmpeg`, `apt install
ffmpeg`), l'export propose aussi MP3 et OGG. Sans lui, WAV seulement.

## Utilisation

1. Choisir une voix dans le menu déroulant (la pastille indique si le modèle est
   déjà installé). Le choix est mémorisé pour les sessions suivantes.
2. Coller ou taper le texte, ou **Ouvrir…** un fichier `.txt` / `.md`
   (UTF-8, repli latin-1).
3. Régler **Vitesse** (0,5× à 2×) et **Volume** (0 à 100 %) si besoin. Les deux
   sont mémorisés et valent aussi bien pour la lecture que pour l'export.
   Ils restent modifiables pendant la lecture : le volume change aussitôt, la
   vitesse à partir du prochain passage.
4. **▶ Lire** — synthétise et joue directement sur la sortie audio. Le passage
   en cours de lecture est surligné dans le texte, qui défile tout seul, et la
   barre de progression avance avec les passages joués.
   Si le modèle manque, l'application propose de le télécharger (barre de
   progression), puis enchaîne toute seule sur la lecture.
5. **⏸ Pause** / **▶ Reprendre** — suspend la lecture aussitôt et la reprend
   exactement où elle en était.
6. **■ Arrêter** — coupe la lecture *et* la génération en cours (< 100 ms),
   même en pause.
7. **⬇ Exporter…** — écrit tout le texte dans un fichier WAV mono 16 bits,
   au fil de la synthèse (mémoire constante, même pour une heure d'audio).
   Le fichier est écrit sous `.part` puis renommé : un export arrêté ne laisse
   rien derrière lui. Si `ffmpeg` est installé, choisir un nom en `.mp3` ou
   `.ogg` convertit le WAV à la fin (le format est déduit de l'extension).

Raccourcis : `Ctrl+O` (ou `Cmd+O` sur macOS) pour ouvrir un fichier,
`Ctrl+Entrée` (ou `Cmd+Entrée`) pour lire, `Espace` pour pause / reprise
(pendant la lecture), `Échap` pour arrêter.

## Voix incluses au catalogue

| Clé | Langue | Licence du jeu de données |
| --- | --- | --- |
| `fr_FR-siwis-medium` | français | [CC BY 4.0](https://datashare.is.ed.ac.uk/handle/10283/2353) |
| `fr_FR-upmc-medium` | français | [CC BY-SA 4.0](https://github.com/marytts/upmc-pierre-data) |
| `en_US-lessac-medium` | anglais US | [licence de recherche Blizzard 2013](https://www.cstr.ed.ac.uk/projects/blizzard/2013/lessac_blizzard2013/license.html) |
| `en_US-amy-medium` | anglais US | [voir mimic3-voices](https://github.com/MycroftAI/mimic3-voices) |
| `en_GB-alba-medium` | anglais GB | [CC BY 4.0](https://datashare.ed.ac.uk/handle/10283/3270) |

Les modèles proviennent de [`rhasspy/piper-voices`](https://huggingface.co/rhasspy/piper-voices)
(~63 Mo par voix « medium »). Pour en ajouter une, il suffit d'une ligne dans
`VOICES` (`src/tts_engine.rs`) : l'URL Hugging Face est déduite de la clé.

Chaque voix hérite des conditions de son jeu de données d'entraînement : elles
varient d'une voix à l'autre et certaines, comme Lessac, passent par une licence
de recherche nominative. À vérifier avant tout usage commercial de l'audio
produit. Le fichier `MODEL_CARD` de chaque voix, sur Hugging Face, fait foi.

## Organisation du code

| Fichier | Rôle |
| --- | --- |
| `src/main.rs` | interface egui uniquement (widgets, threads de travail, états des boutons) |
| `src/tts_engine.rs` | catalogue de voix, téléchargement des modèles, découpage du texte, moteur de synthèse, export |
| `src/piper.rs` | une voix Piper : phonèmes -> identifiants -> inférence ONNX -> audio |
| `src/espeak.rs` | phonémisation via la bibliothèque espeak-ng (FFI) |
| `src/audio_player.rs` | sortie audio en streaming (rodio) + file de préchargement |
| `src/settings.rs` | chemins applicatifs et préférences persistées (JSON) |
| `build.rs` | liaison avec espeak-ng (`ESPEAK_NG_LIB_DIR` pour un autre emplacement) |
| `assets/` | logo (`logo.svg`, source) et icônes dérivées (PNG, `.ico`, `.icns`) |
| `xtask/` | `cargo xtask icons` régénère les icônes depuis le SVG (nécessite `rsvg-convert`) ; `cargo xtask macos-app` installe `Souffleur.app` |

### Points de conception

- **Piper en Rust** : `piper.rs` reprend le pipeline du paquet Python
  `piper-tts` (propositions phonémisées par espeak-ng, ponctuation conservée,
  décomposition NFD, table d'identifiants du modèle, normalisation de chaque
  phrase). Pour un même texte, il produit les mêmes identifiants de phonèmes,
  aux écarts près entre la version d'espeak-ng du système et celle embarquée
  par `piper-tts`.
- **ONNX Runtime figé en 1.28** (`ort = "=2.0.0-rc.13"`) : la 1.31 plante
  (segfault) sur les modèles Piper dès que la phrase dépasse quelques mots.
  Revérifier avec une phrase longue avant toute mise à jour.
- **Textes longs** : `segment_text()` découpe en paragraphes, puis en phrases,
  puis regroupe en segments de ~350 caractères (coupe de secours sur les
  virgules puis les mots). Chaque segment est synthétisé séparément : le son
  démarre en moins d'une seconde quelle que soit la longueur du texte.
- **Suivi du texte** : le découpage travaille sur des intervalles du texte
  d'origine, chaque segment connaît donc sa position exacte dans la zone de
  texte. Les chunks audio sortent du moteur accompagnés de leur index de
  segment ; c'est le thread de lecture, juste avant d'envoyer un chunk à la
  carte son, qui demande le surlignage. Il suit ainsi l'audio réellement joué,
  et non la génération qui a plusieurs secondes d'avance. Le texte est
  verrouillé pendant la lecture pour que les positions restent valables.
- **Préchauffage de la voix** : le modèle sélectionné est chargé dans un thread
  de fond dès le démarrage et à chaque changement de voix (s'il est déjà
  téléchargé). Le premier « Lire » ne paie donc plus les ~0,6 s de chargement.
  `PiperEngine::load` est verrouillé et mis en cache : si le clic arrive
  pendant le chargement, il attend simplement la fin.
- **Pas de blanc entre segments** : `prefetch()` génère le segment suivant dans
  un thread pendant que le courant est joué, via un canal borné (mémoire
  constante).
- **Vitesse et volume** : la vitesse est appliquée à la synthèse, relative à
  la cadence propre du modèle (`length_scale = défaut du modèle / vitesse`).
  Le moteur la relit avant chaque segment, d'où un changement possible en
  cours de lecture, effectif au segment suivant (plus les quelques segments
  déjà préchargés). Le volume plafonne à 100 %, Piper normalisant déjà
  l'audio — au-delà il ne resterait que de l'écrêtage. À l'écoute, il est
  appliqué par le lecteur (effet immédiat) ; à l'export, il est figé dans le
  WAV. Les curseurs ne sont gelés que pendant un export.
- **Arrêt instantané** : un seul drapeau atomique coupe à la fois la
  génération et la lecture. La sortie audio n'est manipulée que par le thread
  de lecture, qui envoie de petits blocs (~90 ms) et n'en garde que deux en
  file.
- **Pause** : le thread de lecture met la sortie en pause et attend la reprise
  sur une variable de condition, sans consommer de CPU ; la file de
  préchargement reste pleine, la reprise est donc instantanée. `stop()`
  débloque aussi une lecture en pause.
- **Threads ↔ interface** : les threads de travail ne touchent jamais l'état
  de l'interface ; ils envoient des messages sur un canal vidé au début de
  chaque image, puis demandent un nouvel affichage.

Les modèles et les préférences sont stockés dans le répertoire de données
standard de la plateforme, par exemple
`~/Library/Application Support/souffleur/` sous macOS (le même que l'ancienne
version Python : voix et réglages sont repris). Un ancien répertoire
`piper-tts-gui/` est repris automatiquement au premier lancement (pas de
retéléchargement des voix).

## Licences

Le code de ce dépôt est sous licence MIT (voir `LICENSE`).

Le binaire compilé est lié à des bibliothèques sous leurs propres licences :

| Dépendance | Licence |
| --- | --- |
| [espeak-ng](https://github.com/espeak-ng/espeak-ng) (liée dynamiquement) | GPL-3.0-or-later |
| [ONNX Runtime](https://onnxruntime.ai) via [`ort`](https://github.com/pykeio/ort) | MIT / MIT OR Apache-2.0 |
| [egui / eframe](https://github.com/emilk/egui), [rodio](https://github.com/RustAudio/rodio), [rfd](https://github.com/PolyMeilleur/rfd) | MIT OR Apache-2.0 |

Aucun code espeak-ng n'est versionné ici, mais le binaire en dépend : un
exécutable ou un `Souffleur.app` **distribué** à des tiers serait soumis à la
GPL-3.0. Pour un usage personnel, rien de plus à faire.

Les modèles de voix ont leurs propres conditions, détaillées plus haut.
