use ftdecode::{
    decode::ft8::Ft8Decoder,
    engine::{DecodeSettings, DecodedSignal},
    message::MessageDecoder,
};
use std::sync::atomic::{AtomicBool, Ordering};
fn pcm(raw: &[u8]) -> Vec<f32> {
    raw[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32)
        .collect()
}
fn decode(pcm: &[f32], depth: u8) -> Vec<DecodedSignal> {
    let mut found = Vec::new();
    let settings = DecodeSettings {
        depth,
        ..DecodeSettings::default()
    };
    Ft8Decoder::new()
        .decode(
            pcm,
            &settings,
            &mut MessageDecoder::default(),
            &AtomicBool::new(false),
            &mut |d| found.push(d),
        )
        .unwrap();
    found
}
#[test]
fn native_clean_pcm_decodes_all_depths() {
    let pcm = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    for depth in 1..=3 {
        let found = decode(&pcm, depth);
        let d = found
            .iter()
            .find(|d| d.message.text == "CQ K1ABC FN42")
            .unwrap_or_else(|| panic!("depth {depth}: {found:?}"));
        assert!((d.frequency_hz - 1500.0).abs() < 1.0, "{d:?}");
        assert!(d.dt_seconds.abs() < 0.05, "{d:?}");
        let expected_bits =
            b"00000000000000000000000000100000010011011110111100011010100010100001100110001"
                .map(|b| b == b'1');
        assert_eq!(d.source_bits, expected_bits);
        assert!((d.snr_db + 10).abs() <= 2, "{d:?}");
    }
}
#[test]
fn native_weak_pcm_decodes() {
    let found = decode(&pcm(include_bytes!("fixtures/ft8-weak.wav")), 3);
    assert!(
        found.iter().any(|d| d.message.text == "CQ K1ABC FN42"),
        "{found:?}"
    );
}
#[test]
fn native_noise_has_no_decodes() {
    assert!(decode(&pcm(include_bytes!("fixtures/ft8-noise.wav")), 3).is_empty());
}
#[test]
fn shifted_native_recording_tracks_timing() {
    let original = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    for shift in [-6000i32, 12000] {
        let mut shifted = vec![0.0; 180000];
        for (i, &v) in original.iter().enumerate() {
            let j = i as i32 + shift;
            if (0..180000).contains(&j) {
                shifted[j as usize] = v
            }
        }
        let found = decode(&shifted, 3);
        let d = found
            .iter()
            .find(|d| d.message.text == "CQ K1ABC FN42")
            .unwrap_or_else(|| panic!("{found:?}"));
        assert!(
            (d.dt_seconds - shift as f32 / 12000.0).abs() < 0.05,
            "{d:?}"
        );
    }
}
#[test]
fn callback_can_cancel_remaining_work() {
    let pcm = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    let cancel = AtomicBool::new(false);
    let mut found = 0;
    let stats = Ft8Decoder::new()
        .decode(
            &pcm,
            &DecodeSettings::default(),
            &mut MessageDecoder::default(),
            &cancel,
            &mut |_| {
                found += 1;
                cancel.store(true, Ordering::Relaxed)
            },
        )
        .unwrap();
    assert_eq!(found, 1);
    assert!(stats.cancelled);
}

#[test]
fn overlapping_native_signals_need_successive_subtraction() {
    let found = decode(&pcm(include_bytes!("fixtures/ft8-mixture.wav")), 3);
    for expected in ["K1ABC W9XYZ EN37", "CQ K1ABC FN42"] {
        assert!(
            found.iter().any(|d| d.message.text == expected),
            "missing {expected}: {found:?}"
        );
    }
    assert_eq!(found.len(), 2, "{found:?}");
}

#[test]
fn overlapping_weak_signal_recovers_with_metric_fallback() {
    let found = decode(&pcm(include_bytes!("fixtures/ft8-metric-fallback.wav")), 3);
    let weak = found
        .iter()
        .find(|d| d.message.text == "K1ABC W9XYZ EN37")
        .expect("weak signal must survive the adjacent stronger transmission");
    assert!(!weak.assisted);
    assert!(weak.diagnostics.as_ref().unwrap().pass > 1);
    assert!((weak.frequency_hz - 1500.0).abs() < 1.0);
    assert!((weak.dt_seconds - 0.4).abs() < 0.05);
    assert!(found.iter().any(|d| d.message.text == "CQ K9AN EN50"));
    assert_eq!(found.len(), 2, "unexpected message from synthetic input");
}

#[test]
fn refined_frequency_respects_requested_bounds_including_edges() {
    let audio = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    for (low_hz, high_hz, expected) in [
        (1449.0, 1499.0, false),
        (1501.0, 1551.0, false),
        (1475.0, 1525.0, true),
        (1500.0, 1550.0, true),
        (1450.0, 1500.0, true),
    ] {
        let settings = DecodeSettings {
            low_hz,
            high_hz,
            ..DecodeSettings::default()
        };
        let mut found = Vec::new();
        Ft8Decoder::new()
            .decode(
                &audio,
                &settings,
                &mut MessageDecoder::default(),
                &AtomicBool::new(false),
                &mut |d| found.push(d),
            )
            .unwrap();
        assert!(
            found
                .iter()
                .all(|d| (low_hz..=high_hz).contains(&d.frequency_hz)),
            "range {low_hz}..{high_hz}: {found:?}"
        );
        assert_eq!(
            found.iter().any(|d| d.message.text == "CQ K1ABC FN42"),
            expected,
            "range {low_hz}..{high_hz}: {found:?}"
        );
    }
}
