// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{decode::ft4, engine, message};
use std::sync::atomic::AtomicBool;
fn pcm(bytes: &[u8]) -> Vec<f32> {
    let mut p = 12;
    while p + 8 <= bytes.len() {
        let n = u32::from_le_bytes(bytes[p + 4..p + 8].try_into().unwrap()) as usize;
        if &bytes[p..p + 4] == b"data" {
            return bytes[p + 8..p + 8 + n]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|x| i16::from_le_bytes([x[0], x[1]]) as f32)
                .take(72576)
                .collect();
        }
        p += 8 + n + (n % 2);
    }
    panic!("missing WAV data")
}
#[test]
fn native_clean_audio() {
    let audio = pcm(include_bytes!("fixtures/ft4-clean.wav"));
    let mut found = Vec::new();
    let stats = ft4::Ft4Decoder::new()
        .decode(
            &audio,
            &engine::DecodeSettings::default(),
            &mut message::MessageDecoder::default(),
            &AtomicBool::new(false),
            &mut |d| found.push(d),
        )
        .unwrap();
    assert_eq!(stats.decoded, 1);
    assert_eq!(found[0].message.text, "CQ K1ABC FN42");
    assert!((found[0].frequency_hz - 1500.0).abs() < 2.0);
    assert!(
        found[0].dt_seconds.abs() < 0.06,
        "dt {}",
        found[0].dt_seconds
    );
}
#[test]
fn native_weak_and_noise() {
    for (bytes, expected) in [
        (include_bytes!("fixtures/ft4-weak.wav").as_slice(), 1),
        (include_bytes!("fixtures/ft4-noise.wav").as_slice(), 0),
    ] {
        let mut found = Vec::new();
        let stats = ft4::Ft4Decoder::new()
            .decode(
                &pcm(bytes),
                &engine::DecodeSettings::default(),
                &mut message::MessageDecoder::default(),
                &AtomicBool::new(false),
                &mut |d| found.push(d),
            )
            .unwrap();
        assert_eq!(stats.decoded, expected);
        if expected > 0 {
            assert_eq!(found[0].message.text, "CQ K1ABC FN42");
        }
    }
}
#[test]
fn timing_depth_and_cancellation() {
    let audio = pcm(include_bytes!("fixtures/ft4-clean.wav"));
    for shift in [-9000_i32, -2400, 12000] {
        let shifted: Vec<f32> = (0..72576)
            .map(|i| audio.get((i - shift) as usize).copied().unwrap_or(0.0))
            .collect();
        let mut found = Vec::new();
        let settings = engine::DecodeSettings {
            depth: 1,
            ..Default::default()
        };
        ft4::Ft4Decoder::new()
            .decode(
                &shifted,
                &settings,
                &mut message::MessageDecoder::default(),
                &AtomicBool::new(false),
                &mut |d| found.push(d),
            )
            .unwrap();
        assert_eq!(found.len(), 1, "shift {shift}");
        assert!((found[0].dt_seconds - shift as f32 / 12000.0).abs() < 0.06);
    }
    let stats = ft4::Ft4Decoder::new()
        .decode(
            &audio,
            &engine::DecodeSettings::default(),
            &mut message::MessageDecoder::default(),
            &AtomicBool::new(true),
            &mut |_| panic!("cancelled callback"),
        )
        .unwrap();
    assert!(stats.cancelled);
}
#[test]
fn overlapping_native_signals() {
    let first = pcm(include_bytes!("fixtures/ft4-clean.wav"));
    let second = pcm(include_bytes!("fixtures/ft4-overlap.wav"));
    let mixed: Vec<f32> = first
        .iter()
        .zip(second)
        .map(|(a, b)| a + b * 0.65)
        .collect();
    let mut found = Vec::new();
    ft4::Ft4Decoder::new()
        .decode(
            &mixed,
            &engine::DecodeSettings::default(),
            &mut message::MessageDecoder::default(),
            &AtomicBool::new(false),
            &mut |d| found.push(d),
        )
        .unwrap();
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.iter().any(|d| d.message.text == "CQ K1ABC FN42"));
    assert!(found.iter().any(|d| d.message.text == "CQ W9XYZ EN37"));
}

#[test]
fn search_range_applies_to_lowest_tone_not_signal_center() {
    let audio = pcm(include_bytes!("fixtures/ft4-clean.wav"));
    for (low, high, expected) in [
        (1450.0, 1510.0, 1),
        (1495.0, 1550.0, 1),
        (1520.0, 1600.0, 0),
    ] {
        let settings = engine::DecodeSettings {
            low_hz: low,
            high_hz: high,
            ..Default::default()
        };
        let mut found = Vec::new();
        ft4::Ft4Decoder::new()
            .decode(
                &audio,
                &settings,
                &mut message::MessageDecoder::default(),
                &AtomicBool::new(false),
                &mut |d| found.push(d),
            )
            .unwrap();
        assert_eq!(found.len(), expected, "range {low}..{high}: {found:?}");
    }
}

#[test]
fn audio_band_edges_and_callback_cancellation() {
    for (bytes, frequency) in [
        (include_bytes!("fixtures/ft4-dc.wav").as_slice(), 0.0),
        (include_bytes!("fixtures/ft4-near-dc.wav").as_slice(), 5.0),
        (include_bytes!("fixtures/ft4-low.wav").as_slice(), 100.0),
        (include_bytes!("fixtures/ft4-high.wav").as_slice(), 4950.0),
    ] {
        let settings = engine::DecodeSettings {
            low_hz: (frequency - 30.0_f32).max(0.0),
            high_hz: (frequency + 30.0_f32).clamp(50.0, 5000.0),
            depth: 1,
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        let mut count = 0;
        let stats = ft4::Ft4Decoder::new()
            .decode(
                &pcm(bytes),
                &settings,
                &mut message::MessageDecoder::default(),
                &cancel,
                &mut |d| {
                    assert_eq!(d.message.text, "CQ K1ABC FN42");
                    assert!((d.frequency_hz - frequency).abs() < 2.0);
                    count += 1;
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                },
            )
            .unwrap();
        assert_eq!(count, 1, "frequency {frequency}");
        assert!(
            stats.cancelled,
            "callback cancellation must reach done status"
        );
    }
}

#[test]
fn full_band_noise_and_direct_api_validation() {
    let settings = engine::DecodeSettings {
        low_hz: 0.0,
        high_hz: 5000.0,
        ..Default::default()
    };
    let audio = pcm(include_bytes!("fixtures/ft4-noise.wav"));
    let mut decoder = ft4::Ft4Decoder::new();
    let stats = decoder
        .decode(
            &audio,
            &settings,
            &mut message::MessageDecoder::default(),
            &AtomicBool::new(false),
            &mut |_| panic!("noise accepted"),
        )
        .unwrap();
    assert_eq!(stats.decoded, 0);
    for bad in [f32::NAN, f32::INFINITY, 32769.0] {
        let mut invalid = vec![0.0; 72576];
        invalid[100] = bad;
        assert!(
            decoder
                .decode(
                    &invalid,
                    &settings,
                    &mut message::MessageDecoder::default(),
                    &AtomicBool::new(false),
                    &mut |_| panic!("invalid audio accepted")
                )
                .is_err()
        );
    }
}
