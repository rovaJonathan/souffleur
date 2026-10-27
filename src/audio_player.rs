//! Lecture audio en streaming via rodio (aucun fichier intermédiaire).
//!
//! Le lecteur consomme un itérateur de chunks `AudioChunk` produit par le
//! moteur TTS et les envoie au fil de l'eau vers la sortie audio.
//!
//! Règle de thread : **seul le thread de lecture touche la sortie audio** ;
//! `stop()`, `pause()` et le volume, appelés depuis l'interface, se contentent
//! de modifier un état partagé. Le thread de lecture envoie de petits blocs et
//! relit cet état entre chaque bloc, en gardant très peu d'audio en file
//! d'attente : arrêt, pause et volume sont perçus comme immédiats.

use std::num::NonZero;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use rodio::buffer::SamplesBuffer;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player};

use crate::tts_engine::AudioChunk;

/// Taille des blocs envoyés à la carte son (~93 ms à 22 050 Hz). Compromis
/// entre réactivité et nombre d'appels.
const BLOCK_SAMPLES: usize = 2048;
/// Blocs gardés en file côté sortie audio : assez pour ne jamais manquer de
/// son, assez peu pour que le surlignage suive l'audio à un bloc près.
const QUEUED_BLOCKS: usize = 2;
const POLL: Duration = Duration::from_millis(10);

/// Drapeau d'arrêt partagé avec le moteur TTS : il coupe aussi la génération.
pub type StopFlag = Arc<AtomicBool>;

struct Shared {
    stop: StopFlag,
    // Vrai = lecture en cours, faux = en pause. Le thread de lecture attend
    // dessus sans boucle active, et `stop()` le relève pour le débloquer.
    running: Mutex<bool>,
    resumed: Condvar,
    // Gain appliqué juste avant la sortie (bits d'un f32), relu à chaque bloc.
    volume: AtomicU32,
}

/// Sortie audio mono, ouverte paresseusement au premier chunk. Les clones
/// partagent le même état : l'interface en garde un, le thread de lecture un
/// autre.
#[derive(Clone)]
pub struct AudioPlayer {
    shared: Arc<Shared>,
}

impl Default for AudioPlayer {
    fn default() -> Self {
        Self {
            shared: Arc::new(Shared {
                stop: Arc::new(AtomicBool::new(false)),
                running: Mutex::new(true),
                resumed: Condvar::new(),
                volume: AtomicU32::new(1.0f32.to_bits()),
            }),
        }
    }
}

impl AudioPlayer {
    // -- état ------------------------------------------------------------- //

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.shared.volume.load(Ordering::Relaxed))
    }

    pub fn set_volume(&self, value: f64) {
        let value = value.clamp(0.0, 1.0) as f32;
        self.shared.volume.store(value.to_bits(), Ordering::Relaxed);
    }

    pub fn stopped(&self) -> bool {
        self.shared.stop.load(Ordering::SeqCst)
    }

    pub fn paused(&self) -> bool {
        !*self.running()
    }

    pub fn stop_flag(&self) -> StopFlag {
        self.shared.stop.clone()
    }

    fn running(&self) -> std::sync::MutexGuard<'_, bool> {
        self.shared
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_running(&self, running: bool) {
        *self.running() = running;
        self.shared.resumed.notify_all();
    }

    /// Réarme les drapeaux d'arrêt et de pause avant une nouvelle lecture.
    ///
    /// Appelé depuis l'interface *avant* de démarrer le thread de travail :
    /// un clic sur « Arrêter » ne peut donc jamais être effacé après coup.
    pub fn reset(&self) {
        self.shared.stop.store(false, Ordering::SeqCst);
        self.set_running(true);
    }

    /// Bascule pause/lecture et retourne le nouvel état `paused`.
    pub fn toggle_pause(&self) -> bool {
        let paused = !self.paused();
        self.set_running(!paused);
        paused
    }

    /// Interrompt la lecture (appelable depuis l'interface). Débloque aussi
    /// une lecture en pause, qui voit alors le drapeau d'arrêt.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.set_running(true);
    }

    // -- lecture ----------------------------------------------------------- //

    /// Bloquant : à appeler depuis un thread de travail, jamais depuis l'UI.
    ///
    /// Retourne quand tout a été joué, juste après un `stop()`, ou à la
    /// première erreur remontée par `chunks`.
    pub fn play(&self, chunks: impl Iterator<Item = Result<AudioChunk>>) -> Result<()> {
        let mut output: Option<(MixerDeviceSink, Player)> = None;
        let result = (|| {
            for chunk in chunks {
                let chunk = chunk?;
                if self.stopped() {
                    break;
                }
                if chunk.samples.is_empty() {
                    continue;
                }
                let (_, player) = match &mut output {
                    Some(output) => output,
                    empty => empty.insert(open_output()?),
                };
                if !self.write_chunk(player, &chunk) {
                    break;
                }
            }
            Ok(())
        })();
        if let Some((_device, player)) = output {
            if self.stopped() || result.is_err() {
                player.stop(); // arrêt demandé : on coupe net, file jetée
            } else {
                // Fin normale : on laisse la file se vider.
                while !player.empty() && self.hold(&player) {
                    thread::sleep(POLL);
                }
            }
        }
        result
    }

    /// Envoie un chunk par petits blocs. Retourne false si l'arrêt est demandé.
    fn write_chunk(&self, player: &Player, chunk: &AudioChunk) -> bool {
        let rate = NonZero::new(chunk.sample_rate).unwrap_or(NonZero::<u32>::MIN);
        for block in chunk.samples.chunks(BLOCK_SAMPLES) {
            if !self.hold(player) {
                return false;
            }
            let samples: Vec<f32> = block.iter().map(|&s| f32::from(s) / 32768.0).collect();
            player.append(SamplesBuffer::new(NonZero::<u16>::MIN, rate, samples));
            // La file bornée régule naturellement la génération.
            while player.len() > QUEUED_BLOCKS {
                if !self.hold(player) {
                    return false;
                }
                thread::sleep(POLL);
            }
        }
        true
    }

    /// Applique pause et volume courants. Bloque tant que la lecture est en
    /// pause ; retourne false si l'arrêt est demandé.
    ///
    /// En pause, la sortie reste ouverte et la file de préchargement pleine :
    /// la reprise est immédiate.
    fn hold(&self, player: &Player) -> bool {
        if self.paused() {
            player.pause();
            let mut running = self.running();
            while !*running {
                running = self
                    .shared
                    .resumed
                    .wait(running)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            drop(running);
            player.play();
        }
        player.set_volume(self.volume());
        !self.stopped()
    }
}

fn open_output() -> Result<(MixerDeviceSink, Player)> {
    let mut device = DeviceSinkBuilder::open_default_sink()
        .map_err(|e| anyhow!("Sortie audio indisponible : {e}"))?;
    device.log_on_drop(false);
    let player = Player::connect_new(device.mixer());
    Ok((device, player))
}

/// Génère les éléments en avance dans un thread, via une file bornée.
///
/// Sans cela, la synthèse du chunk N+1 ne démarrerait qu'une fois le chunk N
/// entièrement joué : on entendrait un blanc entre les segments. La file
/// bornée évite aussi de tout garder en mémoire sur un texte très long.
///
/// `produce` reçoit une fonction d'émission, qui retourne false quand le
/// consommateur est parti : il doit alors s'arrêter. Son erreur éventuelle
/// est relayée au consommateur, après les éléments déjà produits.
pub fn prefetch<T: Send + 'static>(
    size: usize,
    stop: StopFlag,
    produce: impl FnOnce(&mut dyn FnMut(T) -> bool) -> Result<()> + Send + 'static,
) -> impl Iterator<Item = Result<T>> {
    let (sender, receiver) = mpsc::sync_channel::<Result<T>>(size);
    thread::Builder::new()
        .name("tts-prefetch".into())
        .spawn(move || {
            if let Err(error) = produce(&mut |item| sender.send(Ok(item)).is_ok()) {
                let _ = sender.send(Err(error));
            }
        })
        .expect("création du thread de préchargement");

    std::iter::from_fn(move || {
        loop {
            match receiver.recv_timeout(Duration::from_millis(200)) {
                Ok(item) => return Some(item),
                Err(RecvTimeoutError::Timeout) if !stop.load(Ordering::SeqCst) => continue,
                Err(_) => return None, // arrêt demandé, ou producteur terminé
            }
        }
    })
}
