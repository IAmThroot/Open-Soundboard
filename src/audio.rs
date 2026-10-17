use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use cpal::traits::HostTrait;
use crossbeam_channel::{Receiver, Sender};
use log::{debug, info, warn};
use rodio::{mixer::Mixer, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

use crate::devices::{device_display_name, find_device_by_name, SYSTEM_DEFAULT_LABEL};
use crate::model::SlotId;

pub const TEST_SOUND_ID: SlotId = SlotId::MAX;

const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug)]
pub struct TrimSpec {
    pub start_frac: f32,
    pub end_frac: f32,
    pub duration_secs: f32,
}

#[derive(Debug)]
pub enum AudioCommand {
    Play {
        id: SlotId,
        path: PathBuf,
        trim: Option<TrimSpec>,
    },

    PlayTest,
    Stop(SlotId),
    StopAll,

    SetDevice(String),
    SetVolume(f32),
}

#[derive(Debug)]
pub enum AudioEvent {
    Started(SlotId),

    Finished(SlotId),
    FileMissing {
        id: SlotId,
        path: PathBuf,
    },
    DecodeError {
        id: SlotId,
        message: String,
    },

    DeviceReady {
        requested: String,
        actual: String,
        fallback: Option<String>,
    },

    StreamError(String),
}

pub fn spawn_audio_engine(
    initial_device: String,
    volume: f32,
) -> (Sender<AudioCommand>, Receiver<AudioEvent>) {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AudioCommand>();
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<AudioEvent>();

    std::thread::Builder::new()
        .name("audio-engine".to_owned())
        .spawn(move || {
            run_engine(initial_device, volume, cmd_rx, event_tx);
        })
        .expect("failed to spawn audio engine thread");

    (cmd_tx, event_rx)
}

struct Engine {
    cmd_rx: Receiver<AudioCommand>,
    event_tx: Sender<AudioEvent>,

    sink: Option<MixerDeviceSink>,
    mixer: Option<Mixer>,

    players: HashMap<SlotId, Player>,
    current_device: String,
    volume: f32,
}

fn run_engine(
    initial_device: String,
    volume: f32,
    cmd_rx: Receiver<AudioCommand>,
    event_tx: Sender<AudioEvent>,
) {
    let mut engine = Engine {
        cmd_rx,
        event_tx,
        sink: None,
        mixer: None,
        players: HashMap::new(),
        current_device: initial_device.clone(),
        volume: volume.clamp(0.0, 1.0),
    };

    engine.open_device(&initial_device);

    loop {
        match engine.cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(cmd) => {
                engine.handle(cmd);
                while let Ok(cmd) = engine.cmd_rx.try_recv() {
                    engine.handle(cmd);
                }
                engine.poll_finished();
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => engine.poll_finished(),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                info!("UI hung up; audio engine shutting down.");
                break;
            }
        }
    }
}

impl Engine {
    fn emit(&self, event: AudioEvent) {
        if self.event_tx.send(event).is_err() {
            warn!("UI is gone; audio event dropped.");
        }
    }

    fn open_device(&mut self, requested: &str) {
        self.players.clear();

        match open_sink_for(requested, &self.event_tx) {
            Ok(opened) => {
                info!(
                    "Audio device open: requested={requested:?} actual={:?}",
                    opened.actual_name
                );
                let mixer = opened.sink.mixer().clone();
                let mut sink = opened.sink;
                sink.log_on_drop(false);
                self.sink = Some(sink);
                self.mixer = Some(mixer);
                self.current_device = requested.to_owned();
                self.emit(AudioEvent::DeviceReady {
                    requested: requested.to_owned(),
                    actual: opened.actual_name,
                    fallback: opened.fallback_note,
                });
            }
            Err(message) => {
                warn!("Failed to open audio device {requested:?}: {message}");
                self.sink = None;
                self.mixer = None;
                self.emit(AudioEvent::DeviceReady {
                    requested: requested.to_owned(),
                    actual: String::new(),
                    fallback: Some(message),
                });
            }
        }
    }

    fn handle(&mut self, cmd: AudioCommand) {
        match cmd {
            AudioCommand::Play { id, path, trim } => self.play_file(id, path, trim),
            AudioCommand::PlayTest => self.play_test_tone(),
            AudioCommand::Stop(id) => {
                self.players.remove(&id);
                self.emit(AudioEvent::Finished(id));
            }
            AudioCommand::StopAll => {
                let ids: Vec<SlotId> = self.players.keys().copied().collect();
                self.players.clear();
                for id in ids {
                    self.emit(AudioEvent::Finished(id));
                }
            }
            AudioCommand::SetDevice(name) => {
                if self.sink.is_some() && name == self.current_device {
                    return;
                }
                self.open_device(&name);
            }
            AudioCommand::SetVolume(volume) => {
                self.volume = volume.clamp(0.0, 1.0);
                for player in self.players.values() {
                    player.set_volume(self.volume);
                }
            }
        }
    }

    fn play_file(&mut self, id: SlotId, path: PathBuf, trim: Option<TrimSpec>) {
        debug!("Play requested: slot={id} path={}", path.display());

        let file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.emit(AudioEvent::FileMissing { id, path });
                return;
            }
            Err(err) => {
                self.emit(AudioEvent::DecodeError {
                    id,
                    message: format!("Could not read file: {err}"),
                });
                return;
            }
        };

        let decoder = match Decoder::new(file) {
            Ok(decoder) => decoder,
            Err(err) => {
                self.emit(AudioEvent::DecodeError {
                    id,
                    message: format!(
                        "That file could not be played (it may be damaged or use an \
                         unsupported format): {err}"
                    ),
                });
                return;
            }
        };

        let Some(mixer) = &self.mixer else {
            self.emit(AudioEvent::DecodeError {
                id,
                message: "No sound device is open. Pick another device above.".to_owned(),
            });
            return;
        };

        self.players.remove(&id);

        let player = Player::connect_new(mixer);
        player.set_volume(self.volume);
        match trim.and_then(|t| {
            crate::waveform::trim_durations(t.start_frac, t.end_frac, t.duration_secs)
        }) {
            Some((skip, take)) => {
                debug!("Playing slot={id} trimmed: skip={skip:?} take={take:?}.");
                player.append(decoder.skip_duration(skip).take_duration(take));
            }
            None => player.append(decoder),
        }
        self.players.insert(id, player);
        self.emit(AudioEvent::Started(id));
    }

    fn play_test_tone(&mut self) {
        let Some(mixer) = &self.mixer else {
            self.emit(AudioEvent::DecodeError {
                id: TEST_SOUND_ID,
                message: "No sound device is open. Pick another device above.".to_owned(),
            });
            return;
        };

        self.players.remove(&TEST_SOUND_ID);

        let tone = rodio::source::SineWave::new(660.0)
            .take_duration(Duration::from_millis(350))
            .amplify(0.35);

        let player = Player::connect_new(mixer);
        player.set_volume(self.volume);
        player.append(tone);
        self.players.insert(TEST_SOUND_ID, player);
        self.emit(AudioEvent::Started(TEST_SOUND_ID));
    }

    fn poll_finished(&mut self) {
        if self.players.is_empty() {
            return;
        }
        let done: Vec<SlotId> = self
            .players
            .iter()
            .filter(|(_, player)| player.empty())
            .map(|(id, _)| *id)
            .collect();
        for id in done {
            self.players.remove(&id);
            self.emit(AudioEvent::Finished(id));
        }
    }
}

struct OpenedSink {
    sink: MixerDeviceSink,

    actual_name: String,

    fallback_note: Option<String>,
}

fn open_sink_for(requested: &str, event_tx: &Sender<AudioEvent>) -> Result<OpenedSink, String> {
    if requested == SYSTEM_DEFAULT_LABEL {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "Windows reported no default sound device.".to_owned())?;
        let actual = device_display_name(&device).unwrap_or_else(|| "Default Device".to_owned());
        let sink = open_stream(&device, event_tx)
            .map_err(|err| format!("Could not open the default sound device: {err}"))?;
        return Ok(OpenedSink {
            sink,
            actual_name: actual,
            fallback_note: None,
        });
    }

    if let Some(device) = find_device_by_name(requested) {
        let sink = open_stream(&device, event_tx)
            .map_err(|err| format!("Could not open {requested:?}: {err}"))?;
        return Ok(OpenedSink {
            sink,
            actual_name: requested.to_owned(),
            fallback_note: None,
        });
    }

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| format!("{requested:?} was not found, and there is no default device."))?;
    let actual = device_display_name(&device).unwrap_or_else(|| "Default Device".to_owned());
    let sink = open_stream(&device, event_tx)
        .map_err(|err| format!("{requested:?} was not found; default device also failed: {err}"))?;
    Ok(OpenedSink {
        sink,
        actual_name: actual.clone(),
        fallback_note: Some(format!(
            "{requested:?} was not available, so the sound was routed to {actual:?} instead."
        )),
    })
}

fn open_stream(
    device: &cpal::Device,
    event_tx: &Sender<AudioEvent>,
) -> Result<MixerDeviceSink, String> {
    let err_tx = event_tx.clone();
    let callback = move |err: cpal::Error| {
        let _ = err_tx.send(AudioEvent::StreamError(format!(
            "The sound device reported a problem (it may have been unplugged): {err}"
        )));
    };

    DeviceSinkBuilder::from_device(device.clone())
        .map_err(|err| err.to_string())?
        .with_error_callback(callback)
        .open_sink_or_fallback()
        .map_err(|err| err.to_string())
}

#[allow(dead_code)]
fn drain(rx: &Receiver<AudioEvent>) -> Vec<AudioEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn next_event(rx: &Receiver<AudioEvent>, timeout: Duration) -> Option<AudioEvent> {
        rx.recv_timeout(timeout).ok()
    }

    fn write_test_wav(path: &std::path::Path, millis: u32) {
        let sample_rate = 44_100u32;
        let samples = sample_rate * millis / 1000;
        let mut data = Vec::with_capacity(44 + samples as usize * 2);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + samples * 2).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&sample_rate.to_le_bytes());
        data.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&(samples * 2).to_le_bytes());
        for n in 0..samples {
            let t = n as f32 / sample_rate as f32;
            let sample = (f32::sin(t * 440.0 * 2.0 * std::f32::consts::PI) * 10_000.0) as i16;
            data.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(path, data).expect("write test wav");
    }

    #[test]
    fn missing_file_reports_without_needing_a_device() {
        let (tx, rx) = spawn_audio_engine(SYSTEM_DEFAULT_LABEL.to_owned(), 0.7);

        std::thread::sleep(Duration::from_millis(300));
        drain(&rx);

        tx.send(AudioCommand::Play {
            id: 7,
            path: PathBuf::from("/definitely/not/here-soundboard-test.mp3"),
            trim: None,
        })
        .unwrap();

        match next_event(&rx, Duration::from_secs(3)) {
            Some(AudioEvent::FileMissing { id, .. }) => assert_eq!(id, 7),
            other => panic!("expected FileMissing, got {other:?}"),
        }
    }

    #[test]
    fn garbage_file_reports_decode_error_without_needing_a_device() {
        let dir = std::env::temp_dir().join("open-soundboard-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("junk.mp3");
        std::fs::write(&junk, b"this is not audio at all").unwrap();

        let (tx, rx) = spawn_audio_engine(SYSTEM_DEFAULT_LABEL.to_owned(), 0.7);
        std::thread::sleep(Duration::from_millis(300));
        drain(&rx);

        tx.send(AudioCommand::Play {
            id: 9,
            path: junk,
            trim: None,
        })
        .unwrap();

        match next_event(&rx, Duration::from_secs(3)) {
            Some(AudioEvent::DecodeError { id, .. }) => assert_eq!(id, 9),
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }

    #[test]
    fn wav_plays_to_finish_when_a_device_is_available() {
        let dir = std::env::temp_dir().join("open-soundboard-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("beep.wav");
        write_test_wav(&wav, 120);

        let (tx, rx) = spawn_audio_engine(SYSTEM_DEFAULT_LABEL.to_owned(), 0.5);

        let ready = next_event(&rx, Duration::from_secs(5));
        let Some(AudioEvent::DeviceReady { actual, .. }) = ready else {
            eprintln!("SKIP: no DeviceReady from engine");
            return;
        };
        if actual.is_empty() {
            eprintln!("SKIP: no usable audio device on this machine");
            return;
        }

        tx.send(AudioCommand::Play {
            id: 3,
            path: wav,
            trim: None,
        })
        .unwrap();

        match next_event(&rx, Duration::from_secs(5)) {
            Some(AudioEvent::Started(id)) => assert_eq!(id, 3),
            other => panic!("expected Started, got {other:?}"),
        }
        match next_event(&rx, Duration::from_secs(10)) {
            Some(AudioEvent::Finished(id)) => assert_eq!(id, 3),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn stop_all_reports_finished_for_playing_sounds() {
        let dir = std::env::temp_dir().join("open-soundboard-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("long.wav");
        write_test_wav(&wav, 5000);

        let (tx, rx) = spawn_audio_engine(SYSTEM_DEFAULT_LABEL.to_owned(), 0.5);

        match next_event(&rx, Duration::from_secs(5)) {
            Some(AudioEvent::DeviceReady { actual, .. }) if !actual.is_empty() => {}
            _ => {
                eprintln!("SKIP: no usable audio device on this machine");
                return;
            }
        }

        tx.send(AudioCommand::Play {
            id: 11,
            path: wav,
            trim: None,
        })
        .unwrap();
        match next_event(&rx, Duration::from_secs(5)) {
            Some(AudioEvent::Started(id)) => assert_eq!(id, 11),
            other => panic!("expected Started, got {other:?}"),
        }

        tx.send(AudioCommand::StopAll).unwrap();
        match next_event(&rx, Duration::from_secs(5)) {
            Some(AudioEvent::Finished(id)) => assert_eq!(id, 11),
            other => panic!("expected Finished after StopAll, got {other:?}"),
        }
    }
}
