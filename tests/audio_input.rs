// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{
    audio_input::{Channel, ConvertedWav},
    wav::WavReader,
};
use std::{f64::consts::TAU, io::Cursor};

fn float_wav(rate: u32, samples: &[f32]) -> Vec<u8> {
    float_wav_channels(rate, 1, samples)
}

fn float_wav_channels(rate: u32, channels: u16, samples: &[f32]) -> Vec<u8> {
    let data_len = samples.len() as u32 * 4;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend(b"RIFF");
    b.extend((36 + data_len).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(3u16.to_le_bytes());
    b.extend(channels.to_le_bytes());
    b.extend(rate.to_le_bytes());
    b.extend((rate * 4 * u32::from(channels)).to_le_bytes());
    b.extend((4 * channels).to_le_bytes());
    b.extend(32u16.to_le_bytes());
    b.extend(b"data");
    b.extend(data_len.to_le_bytes());
    for sample in samples {
        b.extend(sample.to_le_bytes());
    }
    b
}

#[test]
fn positive_eof_read_validates_short_source_even_when_output_count_is_zero() {
    for frames in [1, 15] {
        let mut source = vec![0.0; frames];
        source[frames - 1] = f32::NAN;
        let wav = WavReader::new(Cursor::new(float_wav(192000, &source))).unwrap();
        let mut converted = ConvertedWav::new(wav, Channel::Left).unwrap();
        assert_eq!(converted.sample_count(), 0);
        assert!(converted.read_samples(0).unwrap().is_empty());
        let err = converted.read_samples(1).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}

#[test]
fn eof_validation_checks_unselected_stereo_samples() {
    let mut source = vec![0.0; 30];
    source[29] = f32::INFINITY;
    let wav = WavReader::new(Cursor::new(float_wav_channels(192000, 2, &source))).unwrap();
    let mut converted = ConvertedWav::new(wav, Channel::Left).unwrap();
    assert_eq!(converted.sample_count(), 0);
    let err = converted.read_samples(1).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

fn signal(rate: u32, frames: usize, frequency: f64, amplitude: f32) -> Vec<f32> {
    (0..frames)
        .map(|i| (TAU * frequency * i as f64 / rate as f64).sin() as f32 * amplitude)
        .collect()
}

fn read_with_maximum(bytes: &[u8], maximum: usize) -> Vec<f32> {
    let wav = WavReader::new(Cursor::new(bytes.to_vec())).unwrap();
    let mut converted = ConvertedWav::new(wav, Channel::Left).unwrap();
    let mut output = Vec::new();
    loop {
        let block = converted.read_samples(maximum).unwrap();
        assert!(block.len() <= maximum.min(12000));
        if block.is_empty() {
            break;
        }
        output.extend(block);
    }
    output
}

fn rms(samples: &[f32]) -> f64 {
    (samples
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt()
}

#[test]
fn twelve_khz_bypass_is_sample_exact_and_reads_are_capped() {
    let source = signal(12000, 24003, 997.0, 0.73125);
    let bytes = float_wav(12000, &source);
    let wav = WavReader::new(Cursor::new(bytes.clone())).unwrap();
    let converted = ConvertedWav::new(wav, Channel::Left).unwrap();
    assert_eq!(converted.sample_count(), 24003);
    assert_eq!(
        read_with_maximum(&bytes, usize::MAX),
        source.iter().map(|x| x * 32768.0).collect::<Vec<_>>()
    );
}

#[test]
fn noninteger_ratio_has_exact_floor_duration_and_is_chunk_invariant() {
    let source = signal(44100, 44101, 1000.0, 0.5);
    let bytes = float_wav(44100, &source);
    let wav = WavReader::new(Cursor::new(bytes.clone())).unwrap();
    let converted = ConvertedWav::new(wav, Channel::Left).unwrap();
    assert_eq!(converted.sample_count(), 12000);

    let single = read_with_maximum(&bytes, 12000);
    let awkward = read_with_maximum(&bytes, 137);
    assert_eq!(single.len(), 12000);
    assert_eq!(single, awkward);
}

#[test]
fn sinc_filter_preserves_passband_and_attenuates_aliases() {
    let pass = float_wav(48000, &signal(48000, 48000, 1000.0, 0.5));
    let stop = float_wav(48000, &signal(48000, 48000, 9000.0, 0.5));
    let pass = read_with_maximum(&pass, 431);
    let stop = read_with_maximum(&stop, 431);
    let pass_middle = rms(&pass[1000..11000]);
    let stop_middle = rms(&stop[1000..11000]);
    let expected = 16384.0 / 2.0f64.sqrt();
    assert!(
        (pass_middle / expected - 1.0).abs() < 0.02,
        "passband rms {pass_middle}"
    );
    assert!(stop_middle < expected * 0.01, "stopband rms {stop_middle}");
}

#[test]
fn maximum_rate_preserves_five_khz_and_rejects_nearby_aliases() {
    let expected = 16384.0 / 2.0f64.sqrt();
    let converted = |frequency| {
        let bytes = float_wav(192000, &signal(192000, 192000, frequency, 0.5));
        let samples = read_with_maximum(&bytes, 997);
        rms(&samples[1000..11000])
    };
    let pass = converted(5000.0);
    assert!((pass / expected - 1.0).abs() < 0.03, "5 kHz rms {pass}");
    for frequency in [7000.0, 9000.0] {
        let stop = converted(frequency);
        assert!(stop < expected * 0.01, "{frequency} Hz rms {stop}");
    }
}

#[test]
fn very_short_and_partial_tail_inputs_never_gain_samples() {
    for rate in [44100, 192000] {
        for frames in [1, 1001, 1023, 1024, 1025, 2047, 3199] {
            let source = vec![0.25; frames];
            let bytes = float_wav(rate, &source);
            let expected = frames as u64 * 12000 / u64::from(rate);
            let wav = WavReader::new(Cursor::new(bytes.clone())).unwrap();
            assert_eq!(
                ConvertedWav::new(wav, Channel::Left)
                    .unwrap()
                    .sample_count(),
                expected,
                "metadata at {rate} Hz with {frames} frames"
            );
            assert_eq!(
                read_with_maximum(&bytes, 11).len() as u64,
                expected,
                "output at {rate} Hz with {frames} frames"
            );
        }
    }
}

#[test]
fn filter_delay_is_compensated_on_the_output_timeline() {
    for rate in [44100, 48000, 192000] {
        let mut source = vec![0.0; rate as usize];
        for target_index in [50usize, 3000, 11950] {
            let start = (target_index as u64 * u64::from(rate)).div_ceil(12000) as usize;
            let end = ((target_index + 1) as u64 * u64::from(rate)).div_ceil(12000) as usize;
            source[start..end].fill(1.0);
        }
        let output = read_with_maximum(&float_wav(rate, &source), 431);
        for target_index in [50usize, 3000, 11950] {
            let start = target_index - 3;
            let peak = output[start..target_index + 4]
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap();
            let actual = start + peak.0;
            assert!(
                actual.abs_diff(target_index) <= 1,
                "rate {rate}: peak {actual}"
            );
        }
    }
}

#[test]
fn arbitrary_rates_preserve_dc_phase_and_five_khz_passband() {
    // Include rates with a long rational phase cycle, just above bypass, and
    // rates near the upper bound, not only integer downsampling factors.
    for rate in [12001, 12347, 22050, 44100, 47999, 96001, 191999, 192000] {
        let frames = rate as usize / 5;
        let constant = read_with_maximum(&float_wav(rate, &vec![0.25; frames]), 137);
        for &sample in &constant[100..constant.len() - 100] {
            assert!(
                (sample - 8192.0).abs() < 0.02,
                "DC gain at {rate}: {sample}"
            );
        }
        for frequency in [1000.0, 5000.0] {
            let source = signal(rate, frames, frequency, 0.5);
            let output = read_with_maximum(&float_wav(rate, &source), 137);
            let error = output[100..output.len() - 100]
                .iter()
                .enumerate()
                .map(|(i, &x)| {
                    let expected = (TAU * frequency * (i + 100) as f64 / 12000.0).sin() * 16384.0;
                    (f64::from(x) - expected).powi(2)
                })
                .sum::<f64>()
                / (output.len() - 200) as f64;
            assert!(
                error.sqrt() / 16384.0 < 0.003,
                "{rate} Hz, {frequency} Hz: error {}",
                error.sqrt()
            );
        }
    }
}

#[test]
fn arbitrary_rates_reject_aliases_and_are_chunk_invariant() {
    for rate in [22050, 44100, 47999, 96001, 191999] {
        let source = signal(rate, rate as usize / 5, 6500.0, 0.5);
        let bytes = float_wav(rate, &source);
        let output = read_with_maximum(&bytes, 137);
        assert_eq!(output, read_with_maximum(&bytes, 12000), "rate {rate}");
        assert!(
            rms(&output[100..output.len() - 100]) < 16384.0 * 0.001,
            "alias at {rate}"
        );
    }
}

#[test]
fn resampler_handles_last_partial_window_and_repeated_eof() {
    for rate in [12001, 44100, 192000] {
        let source = vec![0.25; 4097];
        let bytes = float_wav(rate, &source);
        let expected = source.len() as u64 * 12000 / u64::from(rate);
        let mut converter =
            ConvertedWav::new(WavReader::new(Cursor::new(bytes)).unwrap(), Channel::Left).unwrap();
        let mut total = 0;
        loop {
            let block = converter.read_samples(1).unwrap();
            if block.is_empty() {
                break;
            }
            total += block.len() as u64;
            assert!(block.iter().all(|x| x.is_finite()));
            assert!(converter.read_samples(0).unwrap().is_empty());
        }
        assert_eq!(total, expected);
        for _ in 0..3 {
            assert!(converter.read_samples(100).unwrap().is_empty());
        }
    }
}
