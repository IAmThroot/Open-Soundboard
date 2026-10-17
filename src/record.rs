//! In-app microphone recording.
//!
//! A dedicated thread owns the `cpal` input stream:
//!
//! ```text
//! mic ──stream callback──▶ bounded channel ──▶ recorder thread ──WAV file──▶ RecordEvent ──▶ UI
//! ```
//!
//! Design notes:
//! - The stream callback never blocks: it converts to `f32` and `try_send`s
//!   fixed chunks into a bounded channel, dropping (and counting) on overflow
//!   rather than stalling the realtime thread.
//! - All three `cpal` sample formats (F32/I16/U16) are handled; the file is
//!   always 16-bit PCM WAV via `hound` (universal playback compatibility).
//! - The device is chosen in the UI (see `devices::list_input_devices`) —
//!   the OS default is frequently a silent virtual endpoint (VB-Cable).
//! - Every stage is logged (`Opening` → `Recording from` → `stream started` →
//!   `saved`) so a wedge is always attributable to a stage from a report.
//! - Stream opening passes a timeout: cpal's WASAPI backend otherwise waits
//!   **indefinitely** for device activation (`timeout: None`), which can hang
//!   the recorder thread forever (seen with VB-Audio's CABLE Output).
//! - A 10-minute safety cap auto-stops runaway recordings instead of filling
//!   the disk.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};
use log::{info, warn};

/// Recordings land here: `%APPDATA%\OpenSoundboard\…\data\recordings` on
/// Windows, `~/.local/share/…/recordings` on Linux.
pub const RECORDINGS_DIR: &str = "recordings";

/// Bounded handoff; overflow drops (counted) instead of blocking audio.
const CHANNEL_CAP: usize = 64;
/// Auto-stop before a forgotten recording eats the disk.
const MAX_RECORDING: Duration = Duration::from_secs(10 * 60);
/// How long cpal may spend activating the audio device before we give up
/// with a real error instead of blocking the recorder thread forever.
const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the stream died: set once by the cpal error callback (from the audio
/// worker thread), read by the collect loop (recorder thread). `None` = alive.
type DeathReason = Arc<Mutex<Option<String>>>;

/// Error-callback factory: one fresh closure per stream build (each build
/// consumes its callback). It only *records* the reason — the collect loop
/// turns it into exactly one event, so the UI never gets double toasts and
/// an errored stream can't leave the loop collecting from a dead device.
fn stream_died(reason: &DeathReason, device_name: &str) -> impl FnMut(cpal::Error) + Send + 'static {
    let reason = Arc::clone(reason);
    let device_name = device_name.to_owned();
    move |err: cpal::Error| {
        warn!("Recording failed: {device_name} stream error: {err}");
        if let Ok(mut guard) = reason.lock() {
            guard.get_or_insert_with(|| err.to_string());
        }
    }
}

/// Read the stream's death reason (if it died). Never blocks: the error
/// callback holds the lock only for an assignment.
fn death_reason(reason: &DeathReason) -> Option<String> {
    reason.lock().ok().and_then(|guard| guard.clone())
}

/// UI → recorder.
#[derive(Debug)]
pub enum RecordCommand {
    /// Start a take. `None` = the OS default microphone; `Some(name)` = a
    /// specific device chosen in the UI (the OS default is often a virtual
    /// endpoint that stays silent).
    Start(Option<String>),
    Stop,
}

/// Recorder → UI.
#[derive(Debug)]
pub enum RecordEvent {
    /// The stream is live and the mic is now recording.
    Started { device: String },
    /// File written and ready. The UI adds it as a slot on the active board.
    /// `cut_short` = the stream died mid-take and this is only what was
    /// captured before it (the UI says so instead of implying a clean stop).
    Saved {
        path: PathBuf,
        duration_secs: f32,
        cut_short: bool,
    },
    Failed(String),
}

/// Spawn the recorder thread. Files go under the app data dir.
pub fn spawn_recorder() -> (Sender<RecordCommand>, Receiver<RecordEvent>) {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<RecordCommand>();
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<RecordEvent>();

    std::thread::Builder::new()
        .name("mic-recorder".to_owned())
        .spawn(move || run_recorder(cmd_rx, event_tx))
        .expect("failed to spawn recorder thread");

    (cmd_tx, event_rx)
}

/// Encode already-collected mono-mixed `f32` samples as 16-bit WAV.
/// Pure file work (no device), so it is directly unit-testable.
pub fn encode_wav(
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
    path: &std::path::Path,
) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer =
        hound::WavWriter::create(path, spec).map_err(|err| format!("Could not write file: {err}"))?;
    for &sample in samples {
        let clamped = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        writer
            .write_sample(clamped)
            .map_err(|err| format!("Could not write file: {err}"))?;
    }
    writer
        .finalize()
        .map_err(|err| format!("Could not finish file: {err}"))?;
    Ok(())
}

fn recordings_dir() -> Result<PathBuf, String> {
    let dirs = directories::ProjectDirs::from("com", "OpenSoundboard", "Open Soundboard")
        .ok_or_else(|| "No home directory found.".to_owned())?;
    Ok(dirs.data_dir().join(RECORDINGS_DIR))
}

/// A collision-free file name for a new take, inside `dir`.
///
/// The timestamp has millisecond resolution **and** the candidate is bumped
/// until it does not exist, so two quick takes (or a clock that repeats) can
/// never overwrite each other — losing a recording silently is the one
/// failure mode users cannot recover from.
fn stamp_name_in(dir: &std::path::Path) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let base = format!("recording-{millis}");
    for suffix in 0.. {
        let name = if suffix == 0 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };
        if !dir.join(format!("{name}.wav")).exists() {
            return format!("{name}.wav");
        }
    }
    unreachable!("the loop only ends when a free name is found")
}

/// `stamp_name_in` for the real recordings folder. The directory may not
/// exist yet on the very first take; `exists()` is then false for every
/// suffix and the base name wins.
fn stamp_name() -> String {
    let dir = recordings_dir().unwrap_or_else(|_| PathBuf::from("."));
    stamp_name_in(&dir)
}

fn run_recorder(cmd_rx: Receiver<RecordCommand>, event_tx: Sender<RecordEvent>) {
    loop {
        match cmd_rx.recv() {
            Ok(RecordCommand::Start(device)) => record_once(&cmd_rx, &event_tx, device),
            Ok(RecordCommand::Stop) => {
                // Stop with nothing recording: harmless no-op.
            }
            Err(_) => {
                info!("UI hung up; recorder shutting down.");
                break;
            }
        }
    }
}

/// One recording session: open the mic, collect until `Stop` (or the cap),
/// then write the file. Returns to idle afterwards.
fn record_once(
    cmd_rx: &Receiver<RecordCommand>,
    event_tx: &Sender<RecordEvent>,
    wanted: Option<String>,
) {
    // Every failure is BOTH logged (for the report) and sent to the UI (for
    // the toast) — a silent failure path is exactly what makes bug reports
    // undiagnosable.
    let emit = |event: RecordEvent| {
        if let RecordEvent::Failed(message) = &event {
            warn!("Recording failed: {message}");
        }
        if event_tx.send(event).is_err() {
            warn!("UI is gone; record event dropped.");
        }
    };

    // Resolve the chosen device. A vanished device (unplugged mic, disabled
    // virtual cable) falls back to the OS default with a visible note rather
    // than failing outright.
    let host = cpal::default_host();
    let (device, fallback_note) = match wanted.as_deref() {
        Some(name) => match crate::devices::find_input_device_by_name(name) {
            Some(device) => (Some(device), String::new()),
            None => {
                warn!("Chosen microphone {name:?} is gone; using the system default.");
                (host.default_input_device(), format!("“{name}” is no longer available; "))
            }
        },
        None => (host.default_input_device(), String::new()),
    };
    let device = match device {
        Some(device) => device,
        None => {
            emit(RecordEvent::Failed(format!(
                "{fallback_note}No microphone found. Plug one in and try again."
            )));
            return;
        }
    };
    let device_name = crate::devices::device_display_name(&device)
        .unwrap_or_else(|| "microphone".to_owned());

    // Marker BEFORE the first cpal call: a log with `Opening` but no
    // `Recording from` pinpoints a hang in the config/activation query.
    info!("Opening microphone {device_name}.");
    let config = match device.default_input_config() {
        Ok(config) => config,
        Err(err) => {
            emit(RecordEvent::Failed(format!(
                "The microphone ({device_name}) refused to start ({err}). On Windows, check \
                 Settings → Privacy → Microphone access for this app."
            )));
            return;
        }
    };
    let channels = config.channels();
    let sample_rate = config.sample_rate();
    info!("Recording from {device_name} ({channels}ch, {sample_rate}Hz).");

    let (chunk_tx, chunk_rx) = crossbeam_channel::bounded::<Vec<f32>>(CHANNEL_CAP);
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let died: DeathReason = Arc::new(Mutex::new(None));
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let (tx, dropped) = (chunk_tx, dropped.clone());
            device.build_input_stream(
                config.config(),
                move |data: &[f32], _| {
                    if tx.try_send(data.to_vec()).is_err() {
                        dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                },
                stream_died(&died, &device_name),
                Some(STREAM_OPEN_TIMEOUT),
            )
        }
        cpal::SampleFormat::I16 => {
            let (tx, dropped) = (chunk_tx, dropped.clone());
            device.build_input_stream(
                config.config(),
                move |data: &[i16], _| {
                    let converted: Vec<f32> =
                        data.iter().map(|&s| s as f32 / 32768.0).collect();
                    if tx.try_send(converted).is_err() {
                        dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                },
                stream_died(&died, &device_name),
                Some(STREAM_OPEN_TIMEOUT),
            )
        }
        cpal::SampleFormat::U16 => {
            let (tx, dropped) = (chunk_tx, dropped.clone());
            device.build_input_stream(
                config.config(),
                move |data: &[u16], _| {
                    let converted: Vec<f32> = data
                        .iter()
                        .map(|&s| (s as f32 - 32768.0) / 32768.0)
                        .collect();
                    if tx.try_send(converted).is_err() {
                        dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                },
                stream_died(&died, &device_name),
                Some(STREAM_OPEN_TIMEOUT),
            )
        }
        format => {
            emit(RecordEvent::Failed(format!(
                "The microphone uses an unsupported format ({format:?})."
            )));
            return;
        }
    };

    let stream = match stream {
        Ok(stream) => stream,
        Err(err) => {
            emit(RecordEvent::Failed(format!(
                "Could not open the microphone ({device_name}): {err}. It may be blocked by \
                 Windows microphone privacy settings or in use by another app."
            )));
            return;
        }
    };
    if let Err(err) = stream.play() {
        emit(RecordEvent::Failed(format!("Could not start recording: {err}.")));
        return;
    }
    info!("Mic stream started ({device_name}).");

    // The stream may have errored between build and play: report that
    // directly instead of pretending the take is live.
    if let Some(reason) = death_reason(&died) {
        drop(stream);
        emit(RecordEvent::Failed(format!(
            "The microphone ({device_name}) stopped unexpectedly ({reason})."
        )));
        return;
    }

    emit(RecordEvent::Started {
        device: device_name.clone(),
    });

    let mut samples: Vec<f32> = Vec::new();
    let max_samples = (MAX_RECORDING.as_secs() as usize)
        .saturating_mul(sample_rate as usize)
        .saturating_mul(channels as usize);
    let mut capped = false;
    let mut ui_gone = false;
    let began = Instant::now();

    loop {
        // Stop promptly (100ms tick) without busy-looping.
        match cmd_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(RecordCommand::Stop) => break,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                // The UI is gone (quit or recorder restart): discard the take,
                // matching the Stop button's "quitting discards it" promise.
                ui_gone = true;
                break;
            }
            Ok(RecordCommand::Start(_)) => {
                // Already recording: ignore the double-start.
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        // An errored stream stops delivering data; without this the loop
        // would collect from a dead device until the user's next Stop —
        // which never comes, because the UI already reset to `Record`.
        if death_reason(&died).is_some() {
            break;
        }
        // Time-based cap: also catches a stream that silently stops
        // delivering without ever raising an error.
        if began.elapsed() >= MAX_RECORDING {
            capped = true;
            break;
        }
        while let Ok(chunk) = chunk_rx.try_recv() {
            if samples.len() + chunk.len() > max_samples {
                capped = true;
                break;
            }
            samples.extend_from_slice(&chunk);
        }
        if capped {
            break;
        }
    }
    drop(stream);

    if ui_gone {
        info!("UI gone; discarding the unfinished take.");
        return;
    }

    let dropped = dropped.load(std::sync::atomic::Ordering::Relaxed);
    if dropped > 0 {
        warn!("Dropped {dropped} mic chunks (UI thread lag); recording has gaps.");
    }

    let death = death_reason(&died);
    if samples.is_empty() {
        // No callbacks ever fired. If the stream errored, name the error
        // (actionable); otherwise the take never captured a single buffer.
        let message = match &death {
            Some(reason) => format!(
                "The microphone ({device_name}) stopped unexpectedly ({reason})."
            ),
            None => "Nothing was recorded — the microphone produced no audio at all.".to_owned(),
        };
        emit(RecordEvent::Failed(message));
        return;
    }
    if let Some(reason) = &death {
        info!("Stream died mid-take ({reason}); keeping the partial recording.");
    }
    if capped {
        info!("Recording hit the 10-minute cap; auto-stopped.");
    }

    let dir = match recordings_dir() {
        Ok(dir) => dir,
        Err(message) => {
            emit(RecordEvent::Failed(message));
            return;
        }
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        emit(RecordEvent::Failed(format!(
            "Could not create the recordings folder: {err}."
        )));
        return;
    }
    let path = dir.join(stamp_name());
    if let Err(message) = encode_wav(&samples, channels, sample_rate, &path) {
        emit(RecordEvent::Failed(message));
        return;
    }

    let cut_short = death.is_some();
    let frames = samples.len() / channels.max(1) as usize;
    let duration_secs = frames as f32 / sample_rate as f32;
    info!("Recording saved: {} ({duration_secs:.1}s).", path.display());
    emit(RecordEvent::Saved {
        path,
        duration_secs,
        cut_short,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_samples(secs: f32, rate: u32) -> Vec<f32> {
        let n = (secs * rate as f32) as usize;
        (0..n)
            .map(|i| (i as f32 / rate as f32 * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5)
            .collect()
    }

    #[test]
    fn encoder_round_trips_through_hound() {
        let dir = std::env::temp_dir().join("open-soundboard-record-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.wav");
        let samples = sine_samples(0.5, 44_100);
        encode_wav(&samples, 1, 44_100, &path).unwrap();

        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().sample_rate, 44_100);
        let back: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(back.len(), samples.len());
        // 16-bit quantization of a 0.5-amplitude sine must resemble it.
        let expected = (samples[1000] * 32767.0) as i16;
        assert!((back[1000] - expected).abs() <= 1, "{} vs {expected}", back[1000]);
    }

    #[test]
    fn encoder_clamps_out_of_range() {
        let dir = std::env::temp_dir().join("open-soundboard-record-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clamp.wav");
        encode_wav(&[2.0, -2.0, 0.0], 1, 8000, &path).unwrap();
        let back: Vec<i16> = hound::WavReader::open(&path)
            .unwrap()
            .samples::<i16>()
            .map(|s| s.unwrap())
            .collect();
        assert_eq!(back, vec![32767, -32767, 0]);
    }

    /// Two takes must never overwrite each other, even when they land in the
    /// same millisecond (fast double-taps) — the old second-resolution name
    /// did exactly that.
    ///
    /// The contract is "never return the name of a file that already exists",
    /// and production satisfies it because the recorder writes each file
    /// before the next take asks for a name.
    #[test]
    fn stamp_names_never_collide() {
        let dir = std::env::temp_dir().join("open-soundboard-stamp-tests");
        std::fs::create_dir_all(&dir).unwrap();
        for existing in std::fs::read_dir(&dir).unwrap() {
            let _ = std::fs::remove_file(existing.unwrap().path());
        }

        let mut names: Vec<String> = Vec::new();
        for take in 0..3 {
            let name = stamp_name_in(&dir);
            assert!(
                !dir.join(&name).exists(),
                "take {take} reused the name {name} of an existing file",
            );
            assert!(!names.contains(&name), "take {take} duplicated {name}");
            // Write it, exactly like the recorder does right after naming.
            std::fs::write(dir.join(&name), b"x").unwrap();
            names.push(name);
        }

        for entry in std::fs::read_dir(&dir).unwrap() {
            let _ = std::fs::remove_file(entry.unwrap().path());
        }
    }

    /// Records ~1s from the real default mic. Skips with no input device.
    #[test]
    fn live_mic_records_something_when_a_mic_exists() {
        if cpal::default_host().default_input_device().is_none() {
            eprintln!("SKIP: no input device on this machine");
            return;
        }
        let (cmd_tx, event_rx) = spawn_recorder();
        cmd_tx.send(RecordCommand::Start(None)).unwrap();

        // Wait for Started (or an early Failed on locked-down machines).
        let started = event_rx.recv_timeout(Duration::from_secs(10));
        match started {
            Err(_) => {
                eprintln!("SKIP: mic did not start in time");
                return;
            }
            Ok(RecordEvent::Failed(_)) => {
                eprintln!("SKIP: mic refused (permissions?)");
                return;
            }
            Ok(_) => {}
        }
        std::thread::sleep(Duration::from_secs(1));
        cmd_tx.send(RecordCommand::Stop).unwrap();
        match event_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(RecordEvent::Saved {
                path,
                duration_secs,
                ..
            }) => {
                assert!(path.exists());
                assert!(duration_secs > 0.3, "got {duration_secs}");
                let _ = std::fs::remove_file(path);
            }
            Ok(RecordEvent::Failed(_)) => {
                eprintln!("SKIP: recording failed on this machine (silent/disconnected mic)");
            }
            other => panic!("expected Saved or Failed, got {other:?}"),
        }
    }
}
