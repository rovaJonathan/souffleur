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
2. Coller ou taper le texte.
3. Régler **Vitesse** (0,5× à 2×) et **Volume** (0 à 100 %) si besoin. Les deux
   sont mémorisés et valent aussi bien pour la lecture que pour l'export.
4. **▶ Lire** — synthétise et joue directement sur la sortie audio. Le passage
   en cours de lecture est surligné dans le texte, qui défile tout seul.
   Si le modèle manque, l'application propose de le télécharger (barre de
   progression), puis enchaîne toute seule sur la lecture.
5. **■ Arrêter** — coupe la lecture *et* la génération en cours (< 100 ms).
6. **⬇ Exporter en WAV** — écrit tout le texte dans un fichier mono 16 bits.

Raccourcis : `Ctrl+Entrée` pour lire, `Échap` pour arrêter.

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
- **Pas de blanc entre segments** : `prefetch()` génère le segment suivant dans
  un thread pendant que le courant est joué, via une file bornée (mémoire
  constante).
- **Vitesse et volume** : appliqués à la synthèse (`SynthesisConfig` de Piper)
  et non à la sortie audio, donc identiques à l'écoute et dans le WAV exporté.
  La vitesse est relative à la cadence propre du modèle
  (`length_scale = défaut du modèle / vitesse`) ; le volume plafonne à 100 %,
  Piper normalisant déjà l'audio avant d'appliquer le facteur — au-delà il ne
  resterait que de l'écrêtage. Les curseurs sont gelés pendant le travail : le
  réglage est figé dans l'audio au moment où il est généré.
- **Arrêt instantané** : un seul `threading.Event` coupe à la fois la
  génération et la lecture. Le flux PortAudio n'est manipulé que par le thread
  de lecture (un `abort()` concurrent d'un `write()` bloquant peut figer le
  processus).
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
