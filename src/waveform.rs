use std::path::Path;
use std::time::Duration;

use rodio::{Decoder, Source};

pub const WAVE_BUCKETS: usize = 200;

const MAX_SAMPLES: usize = 60_000_000;

#[derive(Clone, Debug)]
pub struct Waveform {
    pub peaks: Vec<(f32, f32)>,

    pub duration_secs: f32,
}

pub fn build_waveform(path: &Path) -> Result<Waveform, String> {
    let file =
        std::fs::File::open(path).map_err(|err| format!("That file could not be read: {err}"))?;
    let decoder =
        Decoder::new(file).map_err(|err| format!("That file could not be decoded: {err}"))?;

    let channels = decoder.channels().get() as usize;
    let sample_rate = decoder.sample_rate().get();

    let mut mono: Vec<f32> = Vec::new();
    let mut frame = vec![0.0f32; channels];
    let mut filled = 0;
    for sample in decoder {
        frame[filled] = sample;
        filled += 1;
        if filled == channels {
            mono.push(frame.iter().sum::<f32>() / channels as f32);
            filled = 0;
            if mono.len() * channels > MAX_SAMPLES {
                return Err("That file is too long to preview (over ~10 minutes). It can still be played whole.".to_owned());
            }
        }
    }
    if mono.is_empty() {
        return Err("That file contains no audible audio.".to_owned());
    }

    let duration_secs = mono.len() as f32 / sample_rate as f32;
    let peaks = reduce_peaks(&mono, WAVE_BUCKETS);

    Ok(Waveform {
        peaks,
        duration_secs,
    })
}

fn reduce_peaks(mono: &[f32], buckets: usize) -> Vec<(f32, f32)> {
    assert!(buckets > 0);
    if mono.is_empty() {
        return vec![(0.0, 0.0); buckets];
    }
    let per_bucket = (mono.len() as f32 / buckets as f32).max(1.0);
    (0..buckets)
        .map(|b| {
            let start = (b as f32 * per_bucket) as usize;
            let end = (((b + 1) as f32 * per_bucket) as usize)
                .max(start + 1)
                .min(mono.len());
            let slice = &mono[start.min(mono.len() - 1)..end];
            let (mut min, mut max) = (f32::INFINITY, f32::NEG_INFINITY);
            for &s in slice {
                min = min.min(s);
                max = max.max(s);
            }
            (min, max)
        })
        .collect()
}

pub fn trim_durations(
    start_frac: f32,
    end_frac: f32,
    duration_secs: f32,
) -> Option<(Duration, Duration)> {
    let start = start_frac.clamp(0.0, 1.0);
    let end = end_frac.clamp(start, 1.0);
    if start <= 0.0 && end >= 1.0 {
        return None;
    }
    let skip = Duration::from_secs_f32(start * duration_secs);
    let take = Duration::from_secs_f32((end - start) * duration_secs);
    Some((skip, take))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_mono(len: usize, freq: f32, rate: f32) -> Vec<f32> {
        (0..len)
            .map(|i| (i as f32 / rate * freq * 2.0 * std::f32::consts::PI).sin())
            .collect()
    }

    #[test]
    fn peaks_cover_the_full_range() {
        let samples = sine_mono(48_000, 440.0, 48_000.0);
        let peaks = reduce_peaks(&samples, 200);
        assert_eq!(peaks.len(), 200);

        let hi = peaks.iter().map(|(_, max)| *max).fold(0.0f32, f32::max);
        let lo = peaks.iter().map(|(min, _)| *min).fold(0.0f32, f32::min);
        assert!(hi > 0.99, "max peak {hi}");
        assert!(lo < -0.99, "min peak {lo}");
    }

    #[test]
    fn silence_reduces_to_zero_peaks() {
        let peaks = reduce_peaks(&vec![0.0; 1000], 200);
        assert!(peaks.iter().all(|&(min, max)| min == 0.0 && max == 0.0));
    }

    #[test]
    fn empty_input_gives_flat_buckets() {
        assert_eq!(reduce_peaks(&[], 200).len(), 200);
    }

    #[test]
    fn tiny_input_still_fills_all_buckets() {
        let peaks = reduce_peaks(&[0.5, -0.5], 200);
        assert_eq!(peaks.len(), 200);
        assert!(peaks.iter().any(|&(min, _)| min == -0.5));
    }

    #[test]
    fn full_range_trim_is_none() {
        assert!(trim_durations(0.0, 1.0, 10.0).is_none());
        assert!(trim_durations(-0.5, 2.0, 10.0).is_none());
    }

    #[test]
    fn partial_trim_resolves_to_durations() {
        let (skip, take) = trim_durations(0.25, 0.75, 8.0).unwrap();
        assert!((skip.as_secs_f32() - 2.0).abs() < 0.001);
        assert!((take.as_secs_f32() - 4.0).abs() < 0.001);
    }

    #[test]
    fn inverted_trim_clamps_to_empty() {
        let (skip, take) = trim_durations(0.8, 0.2, 10.0).unwrap();
        assert_eq!(take, Duration::ZERO);
        assert!((skip.as_secs_f32() - 8.0).abs() < 0.001);
    }

    fn write_wav(path: &std::path::Path, millis: u32) {
        let rate = 44_100u32;
        let samples = rate * millis / 1000;
        let mut data = Vec::with_capacity(44 + samples as usize * 2);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + samples * 2).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&rate.to_le_bytes());
        data.extend_from_slice(&(rate * 2).to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&(samples * 2).to_le_bytes());
        for n in 0..samples {
            let t = n as f32 / rate as f32;
            let s = (f32::sin(t * 440.0 * 2.0 * std::f32::consts::PI) * 10_000.0) as i16;
            data.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn real_wav_decodes_to_peaks_and_exact_duration() {
        let dir = std::env::temp_dir().join("open-soundboard-waveform-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");
        write_wav(&path, 1000);

        let wave = build_waveform(&path).expect("test wav must decode");
        assert_eq!(wave.peaks.len(), WAVE_BUCKETS);
        assert!(
            (wave.duration_secs - 1.0).abs() < 0.01,
            "got {}",
            wave.duration_secs
        );
        let hi = wave
            .peaks
            .iter()
            .map(|(_, max)| *max)
            .fold(0.0f32, f32::max);
        assert!(hi > 0.2, "peaks should show the tone, got {hi}");
    }

    #[test]
    fn missing_file_is_a_plain_language_error() {
        let err =
            build_waveform(std::path::Path::new("/nope/not-here.wav")).expect_err("must fail");
        assert!(err.contains("could not be read"), "got: {err}");
    }

    #[test]
    fn garbage_file_is_a_plain_language_error() {
        let dir = std::env::temp_dir().join("open-soundboard-waveform-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("junk.mp3");
        std::fs::write(&path, b"not audio").unwrap();
        let err = build_waveform(&path).expect_err("must fail");
        assert!(err.contains("decoded"), "got: {err}");
    }
}
