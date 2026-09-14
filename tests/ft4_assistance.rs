// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{
    assistance::{ApMode, ContactState},
    decode::ft4::Ft4Decoder,
    engine::{DecodeSettings, DecodedSignal},
    message::MessageDecoder,
};
use std::sync::atomic::AtomicBool;
fn pcm(bytes: &[u8]) -> Vec<f32> {
    bytes[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .take(72576)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32)
        .collect()
}
fn settings() -> DecodeSettings {
    let mut s = DecodeSettings {
        assistance: true,
        low_hz: 1475.0,
        high_hz: 1525.0,
        priority_hz: Some(1500.0),
        ..DecodeSettings::default()
    };
    s.ap.mode = ApMode::Auto;
    s.ap.my_call = Some("W9XYZ".into());
    s.ap.dx_call = Some("K1ABC".into());
    s.ap.state = ContactState::RogerReport;
    s
}
fn decode(pcm: &[f32], s: &DecodeSettings) -> Vec<DecodedSignal> {
    let mut out = Vec::new();
    Ft4Decoder::new()
        .decode(
            pcm,
            s,
            &mut MessageDecoder::default(),
            &AtomicBool::new(false),
            &mut |d| out.push(d),
        )
        .unwrap();
    out
}
#[test]
fn contact_ap_recovers_native_weak_audio() {
    let pcm = pcm(include_bytes!("fixtures/ft4-ap-weak.wav"));
    for depth in [2, 3] {
        let mut s = settings();
        s.depth = depth;
        s.ap.mode = ApMode::Off;
        assert!(decode(&pcm, &s).is_empty());
        s.ap.mode = ApMode::Auto;
        let out = decode(&pcm, &s);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].message.text, "W9XYZ K1ABC RR73");
        assert!(out[0].assisted);
        let diagnostics = out[0].diagnostics.as_ref().unwrap();
        assert_eq!(diagnostics.ap_type, Some(3));
        assert!(diagnostics.confidence.is_some_and(|q| q > 0.17));
        if depth == 2 {
            assert_eq!(diagnostics.method, ftdecode::fec::DecodeMethod::Bp);
        }
    }
    for variant in 0..4 {
        let mut s = settings();
        match variant {
            0 => s.depth = 1,
            1 => s.priority_hz = None,
            2 => s.priority_hz = Some(1600.),
            _ => s.assistance = false,
        }
        assert!(decode(&pcm, &s).is_empty(), "variant {variant}");
    }
}
#[test]
fn incorrect_hints_and_noise_do_not_create_messages() {
    let pcm = pcm(include_bytes!("fixtures/ft4-ap-weak.wav"));
    let mut s = settings();
    s.ap.my_call = Some("K9AN".into());
    s.ap.dx_call = Some("G4WJS".into());
    assert!(decode(&pcm, &s).is_empty());
    assert!(decode(&vec![0.; 72576], &settings()).is_empty());
    assert!(
        decode(
            &self::pcm(include_bytes!("fixtures/ft4-noise.wav")),
            &settings()
        )
        .is_empty()
    );
}
