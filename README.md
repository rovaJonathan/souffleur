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
3. **▶ Lire** — synthétise et joue directement sur la sortie audio.
   Si le modèle manque, l'application propose de le télécharger (barre de
   progression), puis enchaîne toute seule sur la lecture.
4. **■ Arrêter** — coupe la lecture *et* la génération en cours (< 100 ms).
5. **⬇ Exporter en WAV** — écrit tout le texte dans un fichier mono 16 bits.

Raccourcis : `Ctrl+Entrée` pour lire, `Échap` pour arrêter.

## Voix incluses au catalogue

| Clé | Langue |
| --- | --- |
| `fr_FR-siwis-medium` | français |
| `fr_FR-upmc-medium` | français |
| `en_US-lessac-medium` | anglais US |
| `en_US-amy-medium` | anglais US |
| `en_GB-alba-medium` | anglais GB |

Les modèles proviennent de [`rhasspy/piper-voices`](https://huggingface.co/rhasspy/piper-voices)
(~63 Mo par voix « medium »). Pour en ajouter une, il suffit d'une ligne dans
`VOICES` (`tts_engine.py`) : l'URL Hugging Face est déduite de la clé.

## Organisation du code

| Fichier | Rôle |
| --- | --- |
| `main.py` | interface Tkinter uniquement (widgets, threads de travail, états des boutons) |
| `tts_engine.py` | catalogue de voix, téléchargement des modèles, découpage du texte, synthèse Piper |
| `audio_player.py` | sortie audio en streaming (sounddevice) + file de préchargement |
| `settings.py` | chemins applicatifs et préférences persistées (JSON) |

### Points de conception

- **Textes longs** : `split_text()` découpe en paragraphes, puis en phrases,
  puis regroupe en segments de ~350 caractères (coupe de secours sur les
  virgules puis les mots). Chaque segment est synthétisé séparément : le son
  démarre en moins d'une seconde quelle que soit la longueur du texte.
- **Pas de blanc entre segments** : `prefetch()` génère le segment suivant dans
  un thread pendant que le courant est joué, via une file bornée (mémoire
  constante).
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
