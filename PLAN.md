# Plan d'améliorations — Souffleur

État des lieux (2026-09-21, M2 Pro, `fr_FR-siwis-medium`) : perf excellente,
rien à corriger. Chargement voix 0,67 s, premier chunk 72 ms, facteur temps
réel 0,028. Ce plan liste des améliorations fonctionnelles, à traiter une par
une dans l'ordre. Cocher au fur et à mesure.

## 1. Suivi visuel du texte lu (mode prompteur)

- [x] Surligner dans `tk.Text` le segment en cours de lecture, défilement auto.
      (branche `feat/suivi-texte`)

**Pourquoi** : colle au nom et à l'usage de l'application. L'infrastructure
existe déjà : `on_segment(index, total, segment)` remonte le texte du segment
via la file `_ui`.

**Comment** :
- `tts_engine.split_text()` retourne aussi les offsets (début, fin) de chaque
  segment dans le texte d'origine (avant normalisation des espaces), ou à défaut
  `main.py` retrouve le segment avec `text.search` sur ses premiers mots.
- Dans `_show_segment_progress` : `tag_remove("current")`, `tag_add("current",
  start, end)`, `see(start)`.
- Style : fond jaune pâle, retiré en fin de lecture ou à l'arrêt.
- Le widget passe en lecture seule pendant la lecture (`state="disabled"`),
  sinon les offsets ne correspondent plus.

Fichiers : `tts_engine.py`, `main.py`. Taille : ~60 lignes.

## 2. Préchauffage de la voix

- [x] Charger le modèle en thread de fond au démarrage et à chaque changement de
      voix (si déjà téléchargée). (branche `feat/prechauffage-voix`)

**Pourquoi** : le premier « Lire » paie 0,67 s de chargement. Préchauffé, la
latence perçue tombe à ~70 ms.

**Comment** : thread daemon appelant `engine.load(key)` depuis
`_restore_preferences` et `_on_voice_selected`. `PiperEngine.load` est déjà
protégé par un verrou et met en cache : aucun changement moteur. Ignorer les
erreurs silencieusement (le vrai chemin d'erreur reste celui de « Lire »).

Fichiers : `main.py`. Taille : ~10 lignes.

## 3. Pause / reprise

- [x] Bouton ⏸ / ▶ pendant la lecture, raccourci `Espace` (hors zone de texte).
      (branche `feat/pause-reprise`)

**Comment** : second `threading.Event` (`pause_event`) dans `AudioPlayer`,
testé dans `_write_chunk` entre deux blocs (attente active courte avec
`wait(0.05)` ou boucle sur l'événement). La file de prefetch reste pleine :
reprise instantanée. `stop()` doit aussi débloquer une pause.

Fichiers : `audio_player.py`, `main.py`. Taille : ~40 lignes.

## 4. Ouvrir un fichier texte

- [ ] Bouton « Ouvrir… » (`Ctrl+O`) pour `.txt` / `.md`, et glisser-déposer si
      possible sans dépendance externe.

**Comment** : `filedialog.askopenfilename`, lecture UTF-8 avec repli latin-1,
remplace le contenu de la zone de texte. Le glisser-déposer natif Tk n'existe
pas sans `tkinterdnd2` : à évaluer, sinon s'en tenir au bouton.

Fichiers : `main.py`. Taille : ~30 lignes.

## 5. Export WAV en streaming

- [ ] Écrire le WAV chunk par chunk au lieu de tout concaténer en mémoire.

**Pourquoi** : 1 h d'audio ≈ 160 Mo en RAM aujourd'hui. Cas rare, mais
l'écriture en flux est simple et libère `concatenate()`.

**Comment** : `write_wav` devient un gestionnaire de contexte ouvrant
`wave.open` et exposant `write(samples)` ; le worker d'export itère sur
`synthesize_text` et écrit au fil de l'eau. Fichier `.part` renommé à la fin,
supprimé si arrêt demandé.

Fichiers : `tts_engine.py`, `main.py`. Taille : ~30 lignes.

## 6. Export MP3 / OGG

- [ ] Proposer un format compressé dans la boîte d'export.

**Comment** : sans dépendance native, passer par `ffmpeg` s'il est présent dans
le `PATH` (`shutil.which`), sinon masquer l'option. Le WAV reste le format par
défaut.

Fichiers : `tts_engine.py`, `main.py`. Taille : ~40 lignes.

## Notes

- Doublon mineur : `main.py:_start_play` appelle `split_text()` juste pour
  compter les segments, puis `synthesize_text` le refait. Le point 1 règle ça
  de fait (les segments calculés côté UI servent au surlignage).
- Chaque point = une branche + une PR, comme les précédentes
  (`feat/<nom-court>`).
