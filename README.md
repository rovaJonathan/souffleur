# Souffleur

Comme au théâtre : il lit votre texte à voix haute, et rien ne sort de la salle.

Petite application de bureau (Tkinter) pour faire parler du texte **en local**,
avec [Piper TTS](https://github.com/rhasspy/piper). Aucune donnée ne quitte la
machine : seul le premier téléchargement d'un modèle de voix utilise le réseau.

## Installation

```bash
python -m venv .venv
source .venv/bin/activate        # Windows : .venv\Scripts\activate
pip install -r requirements.txt
python main.py
```

Sous Linux, `sounddevice` a besoin de PortAudio : `sudo apt install libportaudio2`.

## Utilisation

1. Choisir une voix dans le menu déroulant (le pastille indique si le modèle est
   déjà installé). Le choix est mémorisé pour les sessions suivantes.
2. Coller ou taper le texte, ou **📂 Ouvrir…** un fichier `.txt` / `.md`
   (UTF-8, repli latin-1).
3. Régler **Vitesse** (0,5× à 2×) et **Volume** (0 à 100 %) si besoin. Les deux
   sont mémorisés et valent aussi bien pour la lecture que pour l'export.
   Ils restent modifiables pendant la lecture : le volume change aussitôt, la
   vitesse à partir du prochain passage.
4. **▶ Lire** — synthétise et joue directement sur la sortie audio. Le passage
   en cours de lecture est surligné dans le texte, qui défile tout seul.
   Si le modèle manque, l'application propose de le télécharger (barre de
   progression), puis enchaîne toute seule sur la lecture.
5. **⏸ Pause** / **▶ Reprendre** — suspend la lecture au bloc suivant (~100 ms)
   et la reprend exactement où elle en était.
6. **■ Arrêter** — coupe la lecture *et* la génération en cours (< 100 ms),
   même en pause.
7. **⬇ Exporter en WAV** — écrit tout le texte dans un fichier mono 16 bits,
   au fil de la synthèse (mémoire constante, même pour une heure d'audio).
   Le fichier est écrit sous `.part` puis renommé : un export arrêté ne laisse
   rien derrière lui.

Raccourcis : `Ctrl+O` (ou `Cmd+O` sur macOS) pour ouvrir un fichier,
`Ctrl+Entrée` pour lire, `Espace` pour pause / reprise (pendant la lecture),
`Échap` pour arrêter.

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
`VOICES` (`tts_engine.py`) : l'URL Hugging Face est déduite de la clé.

Chaque voix hérite des conditions de son jeu de données d'entraînement : elles
varient d'une voix à l'autre et certaines, comme Lessac, passent par une licence
de recherche nominative. À vérifier avant tout usage commercial de l'audio
produit. Le fichier `MODEL_CARD` de chaque voix, sur Hugging Face, fait foi.

## Organisation du code

| Fichier | Rôle |
| --- | --- |
| `main.py` | interface Tkinter uniquement (widgets, threads de travail, états des boutons) |
| `tts_engine.py` | catalogue de voix, téléchargement des modèles, découpage du texte, synthèse Piper |
| `audio_player.py` | sortie audio en streaming (sounddevice) + file de préchargement |
| `settings.py` | chemins applicatifs et préférences persistées (JSON) |

### Points de conception

- **Textes longs** : `segment_text()` découpe en paragraphes, puis en phrases,
  puis regroupe en segments de ~350 caractères (coupe de secours sur les
  virgules puis les mots). Chaque segment est synthétisé séparément : le son
  démarre en moins d'une seconde quelle que soit la longueur du texte.
- **Suivi du texte** : le découpage travaille sur des intervalles du texte
  d'origine, chaque segment connaît donc sa position exacte dans la zone de
  texte. Les chunks audio sortent du moteur accompagnés de leur index de
  segment ; c'est le thread de lecture, juste avant d'écrire un chunk sur la
  carte son, qui demande le surlignage. Il suit ainsi l'audio réellement joué,
  et non la génération qui a plusieurs secondes d'avance. Le texte est
  verrouillé pendant la lecture pour que les positions restent valables.
- **Préchauffage de la voix** : le modèle sélectionné est chargé dans un thread
  de fond dès le démarrage et à chaque changement de voix (s'il est déjà
  téléchargé). Le premier « Lire » ne paie donc plus les ~0,7 s de chargement.
  `PiperEngine.load` est verrouillé et mis en cache : si le clic arrive
  pendant le chargement, il attend simplement la fin.
- **Pas de blanc entre segments** : `prefetch()` génère le segment suivant dans
  un thread pendant que le courant est joué, via une file bornée (mémoire
  constante).
- **Vitesse et volume** : la vitesse est appliquée à la synthèse
  (`SynthesisConfig` de Piper), relative à la cadence propre du modèle
  (`length_scale = défaut du modèle / vitesse`). Le moteur la relit avant
  chaque segment, d'où un changement possible en cours de lecture, effectif au
  segment suivant (plus les quelques segments déjà préchargés). Le volume
  plafonne à 100 %, Piper normalisant déjà l'audio — au-delà il ne resterait
  que de l'écrêtage. À l'écoute, il est appliqué par le lecteur bloc par bloc
  (effet en ~100 ms) ; à l'export, il est figé dans le WAV par Piper. Les
  curseurs ne sont gelés que pendant un export.
- **Arrêt instantané** : un seul `threading.Event` coupe à la fois la
  génération et la lecture. Le flux PortAudio n'est manipulé que par le thread
  de lecture (un `abort()` concurrent d'un `write()` bloquant peut figer le
  processus).
- **Pause** : un second événement, « lecture en cours », est testé par le
  thread de lecture entre deux blocs. Baissé, le thread attend dessus sans
  consommer de CPU ; le flux PortAudio reste ouvert et joue du silence, la file
  de préchargement reste pleine, la reprise est donc instantanée. `stop()` lève
  aussi cet événement pour débloquer une lecture en pause.
- **Thread ↔ Tkinter** : les threads de travail ne touchent jamais un widget ;
  ils empilent des callbacks dans une file vidée par `_pump()` côté Tkinter
  (sous Tk 9, un `after()` appelé depuis un autre thread n'est jamais exécuté).

Les modèles et les préférences sont stockés dans le répertoire de données
standard de la plateforme, par exemple
`~/Library/Application Support/souffleur/` sous macOS. Un ancien répertoire
`piper-tts-gui/` est repris automatiquement au premier lancement (pas de
retéléchargement des voix).

## Licences

Le code de ce dépôt est sous licence MIT (voir `LICENSE`).

Il s'appuie sur des dépendances installées séparément par `pip`, sous leurs
propres licences :

| Dépendance | Licence |
| --- | --- |
| [piper-tts](https://github.com/OHF-voice/piper1-gpl) (paquet `piper1-gpl`) | GPL-3.0-or-later |
| [sounddevice](https://github.com/spatialaudio/python-sounddevice) | MIT |
| [numpy](https://numpy.org) | BSD-3-Clause |

Aucun code Piper n'est redistribué ici : c'est une dépendance que l'utilisateur
installe lui-même. Une distribution packagée qui embarquerait Piper (exécutable
autonome) serait, elle, soumise à la GPL-3.0. `onnxruntime` (MIT), tiré par
`piper-tts`, n'impose rien de plus.

Les modèles de voix ont leurs propres conditions, détaillées plus haut.
