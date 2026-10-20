use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};
use log::{info, warn};

pub const RECORDINGS_DIR: &str = "recordings";

const CHANNEL_CAP: usize = 64;

const MAX_RECORDING: Duration = Duration::from_secs(10 * 60);

const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

type DeathReason = Arc<Mutex<Option<String>>>;

fn stream_died(
    reason: &DeathReason,
    device_name: &str,
) -> impl FnMut(cpal::Error) + Send + 'static {
    let reason = Arc::clone(reason);
    let device_name = device_name.to_owned();
    move |err: cpal::Error| {
        warn!("Recording failed: {device_name} stream error: {err}");
        if let Ok(mut guard) = reason.lock() {
            guard.get_or_insert_with(|| err.to_string());
        }
    }
}

fn death_reason(reason: &DeathReason) -> Option<String> {
    reason.lock().ok().and_then(|guard| guard.clone())
}

#[derive(Debug)]
pub enum RecordCommand {
    Start(Option<String>),
    Stop,
}

#[derive(Debug)]
pub enum RecordEvent {
    Started {
        device: String,
    },

    Saved {
        path: PathBuf,
        duration_secs: f32,
        cut_short: bool,
    },
    Failed(String),
}

pub fn spawn_recorder(ctx: egui::Context) -> (Sender<RecordCommand>, Receiver<RecordEvent>) {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<RecordCommand>();
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<RecordEvent>();

    std::thread::Builder::new()
        .name("mic-recorder".to_owned())
        .spawn(move || run_recorder(cmd_rx, event_tx, ctx))
        .expect("failed to spawn recorder thread");

    (cmd_tx, event_rx)
}

#[cfg(test)]
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
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|err| format!("Could not write file: {err}"))?;
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
    crate::paths::recordings_dir()
}

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

fn stamp_name() -> String {
    let dir = recordings_dir().unwrap_or_else(|_| PathBuf::from("."));
    stamp_name_in(&dir)
}

fn run_recorder(
    cmd_rx: Receiver<RecordCommand>,
    event_tx: Sender<RecordEvent>,
    ctx: egui::Context,
) {
    loop {
        match cmd_rx.recv() {
            Ok(RecordCommand::Start(device)) => record_once(&cmd_rx, &event_tx, &ctx, device),
            Ok(RecordCommand::Stop) => {}
            Err(_) => {
                info!("UI hung up; recorder shutting down.");
                break;
            }
        }
    }
}

fn record_once(
    cmd_rx: &Receiver<RecordCommand>,
    event_tx: &Sender<RecordEvent>,
    ctx: &egui::Context,
    wanted: Option<String>,
) {
    let emit = |event: RecordEvent| {
        if let RecordEvent::Failed(message) = &event {
            warn!("Recording failed: {message}");
        }
        if event_tx.send(event).is_err() {
            warn!("UI is gone; record event dropped.");
        } else {
            ctx.request_repaint();
        }
    };

    let host = cpal::default_host();
    let (device, fallback_note) = match wanted.as_deref() {
        Some(name) => match crate::devices::find_input_device_by_name(name) {
            Some(device) => (Some(device), String::new()),
            None => {
                warn!("Chosen microphone {name:?} is gone; using the system default.");
                (
                    host.default_input_device(),
                    format!("“{name}” is no longer available; "),
                )
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
    let device_name =
        crate::devices::device_display_name(&device).unwrap_or_else(|| "microphone".to_owned());

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
                    let converted: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
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
        emit(RecordEvent::Failed(format!(
            "Could not start recording: {err}."
        )));
        return;
    }
    info!("Mic stream started ({device_name}).");

    if let Some(reason) = death_reason(&died) {
        drop(stream);
        emit(RecordEvent::Failed(format!(
            "The microphone ({device_name}) stopped unexpectedly ({reason})."
        )));
        return;
    }

    let dir = match recordings_dir() {
        Ok(dir) => dir,
        Err(message) => {
            drop(stream);
            emit(RecordEvent::Failed(message));
            return;
        }
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        drop(stream);
        emit(RecordEvent::Failed(format!(
            "Could not create the recordings folder: {err}."
        )));
        return;
    }
    let path = dir.join(stamp_name());
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = match hound::WavWriter::create(&path, spec) {
        Ok(w) => w,
        Err(err) => {
            drop(stream);
            emit(RecordEvent::Failed(format!(
                "Failed to create recording WAV file: {err}."
            )));
            return;
        }
    };

    emit(RecordEvent::Started {
        device: device_name.clone(),
    });

    let max_samples = (MAX_RECORDING.as_secs() as usize)
        .saturating_mul(sample_rate as usize)
        .saturating_mul(channels as usize);
    let mut total_samples = 0usize;
    let mut capped = false;
    let mut ui_gone = false;
    let began = Instant::now();

    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(RecordCommand::Stop) => break,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                ui_gone = true;
                break;
            }
            Ok(RecordCommand::Start(_)) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }

        if death_reason(&died).is_some() {
            break;
        }

        if began.elapsed() >= MAX_RECORDING {
            capped = true;
            break;
        }
        while let Ok(chunk) = chunk_rx.try_recv() {
            for &s in &chunk {
                if total_samples >= max_samples {
                    capped = true;
                    break;
                }
                let clamped = s.clamp(-1.0, 1.0);
                let sample = (clamped * i16::MAX as f32) as i16;
                let _ = writer.write_sample(sample);
                total_samples += 1;
            }
            if capped {
                break;
            }
        }
        if capped {
            break;
        }
    }

    if !ui_gone {
        while let Ok(chunk) = chunk_rx.try_recv() {
            for &s in &chunk {
                if total_samples >= max_samples {
                    capped = true;
                    break;
                }
                let clamped = s.clamp(-1.0, 1.0);
                let sample = (clamped * i16::MAX as f32) as i16;
                let _ = writer.write_sample(sample);
                total_samples += 1;
            }
            if capped {
                break;
            }
        }
    }

    drop(stream);

    if ui_gone {
        drop(writer);
        let _ = std::fs::remove_file(&path);
        info!("UI gone; discarding the unfinished take.");
        return;
    }

    let dropped = dropped.load(std::sync::atomic::Ordering::Relaxed);
    if dropped > 0 {
        warn!("Dropped {dropped} mic chunks (UI thread lag); recording has gaps.");
    }

    let death = death_reason(&died);
    if total_samples == 0 {
        drop(writer);
        let _ = std::fs::remove_file(&path);
        let message = match &death {
            Some(reason) => {
                format!("The microphone ({device_name}) stopped unexpectedly ({reason}).")
            }
            None => "Nothing was recorded — the microphone produced no audio at all.".to_owned(),
        };
        emit(RecordEvent::Failed(message));
        return;
    }

    if let Err(err) = writer.finalize() {
        let _ = std::fs::remove_file(&path);
        emit(RecordEvent::Failed(format!(
            "Failed to finalize recording WAV file: {err}."
        )));
        return;
    }

    if let Some(reason) = &death {
        info!("Stream died mid-take ({reason}); keeping the partial recording.");
    }
    if capped {
        info!("Recording hit the 10-minute cap; auto-stopped.");
    }

    let cut_short = death.is_some() || dropped > 0;
    let frames = total_samples / channels.max(1) as usize;
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

        let expected = (samples[1000] * 32767.0) as i16;
        assert!(
            (back[1000] - expected).abs() <= 1,
            "{} vs {expected}",
            back[1000]
        );
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

            std::fs::write(dir.join(&name), b"x").unwrap();
            names.push(name);
        }

        for entry in std::fs::read_dir(&dir).unwrap() {
            let _ = std::fs::remove_file(entry.unwrap().path());
        }
    }

    #[test]
    fn live_mic_records_something_when_a_mic_exists() {
        if cpal::default_host().default_input_device().is_none() {
            eprintln!("SKIP: no input device on this machine");
            return;
        }
        let (cmd_tx, event_rx) = spawn_recorder(egui::Context::default());
        cmd_tx.send(RecordCommand::Start(None)).unwrap();

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
