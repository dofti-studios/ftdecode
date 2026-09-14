// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{
    assistance::{Activity, ApMode},
    decode::ft8_assistance::Ft8ListDecoder,
    engine::{DecodeSettings, DecodedSignal},
    message::MessageDecoder,
    message_encode,
};
use std::sync::atomic::AtomicBool;

fn auto_settings() -> DecodeSettings {
    DecodeSettings {
        assistance: true,
        ap: ftdecode::assistance::ApSettings {
            mode: ApMode::Auto,
            ..Default::default()
        },
        ..DecodeSettings::default()
    }
}

fn pcm(raw: &[u8]) -> Vec<f32> {
    raw[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32)
        .collect()
}
fn saved(text: &str) -> DecodedSignal {
    let bits = message_encode::encode(text).unwrap();
    DecodedSignal {
        source_bits: bits,
        message: MessageDecoder::default().unpack(&bits).unwrap(),
        frequency_hz: 1500.0,
        snr_db: -10,
        dt_seconds: 0.0,
        assisted: false,
        diagnostics: None,
    }
}
fn decode(
    decoder: &mut Ft8ListDecoder,
    pcm: &[f32],
    settings: &DecodeSettings,
) -> Vec<DecodedSignal> {
    decoder
        .decode(
            pcm,
            settings,
            &mut MessageDecoder::default(),
            &AtomicBool::new(false),
        )
        .unwrap()
        .into_iter()
        .map(|c| c.signal)
        .collect()
}
fn targeted() -> DecodeSettings {
    let mut settings = DecodeSettings {
        priority_hz: Some(1500.0),
        ..auto_settings()
    };
    settings.ap.my_call = Some("W9XYZ".into());
    settings.ap.dx_call = Some("K1ABC".into());
    settings.ap.dx_grid = Some("FN42".into());
    settings
}
#[test]
fn a7_recovers_native_audio_from_previous_same_parity_slot() {
    let audio = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    let mut decoder = Ft8ListDecoder::new();
    decoder.begin_slot(100);
    decoder.remember(&saved("CQ K1ABC FN42"));
    decoder.begin_slot(101);
    assert!(decode(&mut decoder, &audio, &auto_settings()).is_empty());
    decoder.begin_slot(102);
    let found = decode(&mut decoder, &audio, &auto_settings());
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].message.text, "CQ K1ABC FN42");
    assert_eq!(found[0].diagnostics.as_ref().unwrap().ap_type, Some(7));
}
#[test]
fn a7_repeated_slot_does_not_advance_history_and_reset_invalidates_it() {
    let audio = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    let mut decoder = Ft8ListDecoder::new();
    decoder.begin_slot(100);
    decoder.remember(&saved("CQ K1ABC FN42"));
    decoder.begin_slot(100);
    assert!(decode(&mut decoder, &audio, &auto_settings()).is_empty());
    decoder.begin_slot(102);
    assert_eq!(decode(&mut decoder, &audio, &auto_settings()).len(), 1);
    decoder.reset();
    decoder.begin_slot(104);
    assert!(decode(&mut decoder, &audio, &auto_settings()).is_empty());
}
#[test]
fn a7_discontinuity_wrong_calls_and_already_decoded_station_are_rejected() {
    let audio = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    for (text, next, remember_current) in [
        ("CQ K1ABC FN42", 104, false),
        ("CQ W9XYZ FN42", 102, false),
        ("CQ K1ABC FN42", 102, true),
    ] {
        let mut decoder = Ft8ListDecoder::new();
        decoder.begin_slot(100);
        decoder.remember(&saved(text));
        decoder.begin_slot(next);
        if remember_current {
            decoder.remember(&saved("CQ K1ABC FN42"));
        }
        assert!(decode(&mut decoder, &audio, &auto_settings()).is_empty());
    }
}
#[test]
fn a8_recovers_native_audio_with_explicit_target() {
    let found = decode(
        &mut Ft8ListDecoder::new(),
        &pcm(include_bytes!("fixtures/ft8-clean.wav")),
        &targeted(),
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].message.text, "CQ K1ABC FN42");
    assert_eq!(found[0].diagnostics.as_ref().unwrap().ap_type, Some(8));
    assert!(found[0].dt_seconds.abs() < 0.03, "{found:?}");
    assert!((found[0].frequency_hz - 1500.0).abs() < 0.3, "{found:?}");
}
#[test]
fn list_assistance_requires_auto_and_contact_requires_rx() {
    let audio = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    for variant in 0..7 {
        let mut settings = targeted();
        match variant {
            0 => settings.priority_hz = None,
            1 => settings.ap.mode = ApMode::Cq,
            2 => settings.assistance = false,
            3 => settings.ap.dx_grid = None,
            4 => settings.ap.my_call = None,
            5 => settings.ap.activity = Activity::Fox,
            _ => settings.ap.activity = Activity::Hound,
        }
        assert!(decode(&mut Ft8ListDecoder::new(), &audio, &settings).is_empty());
    }
}
#[test]
fn wrong_target_and_noise_do_not_generate_messages() {
    let audio = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    let mut settings = targeted();
    settings.priority_hz = Some(1510.0);
    assert!(decode(&mut Ft8ListDecoder::new(), &audio, &settings).is_empty());
    let noise = pcm(include_bytes!("fixtures/ft8-noise.wav"));
    assert!(decode(&mut Ft8ListDecoder::new(), &noise, &targeted()).is_empty());
    assert!(decode(&mut Ft8ListDecoder::new(), &vec![0.0; 180000], &targeted()).is_empty());
}

#[test]
fn native_list_differential_vectors() {
    let reference: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/ft8-list-vectors.json")).unwrap();
    let clean = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    let noise = pcm(include_bytes!("fixtures/ft8-noise.wav"));
    for case in reference["cases"].as_array().unwrap() {
        let kind = case["kind"].as_u64().unwrap() as u8;
        let scale = case["scale"].as_f64().unwrap() as f32;
        let shift = case["shift"].as_i64().unwrap() as i32;
        let audio: Vec<f32> = noise
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let j = i as i32 - shift;
                if (0..180000).contains(&j) {
                    clean[j as usize] * scale + n
                } else {
                    n
                }
            })
            .collect();
        let mut settings = auto_settings();
        let call1 = case["call1"].as_str().unwrap();
        let call2 = case["call2"].as_str().unwrap();
        let grid = case["grid"].as_str().unwrap();
        let frequency = case["rx"].as_f64().unwrap() as f32;
        let mut decoder = Ft8ListDecoder::new();
        if kind == 7 {
            decoder.begin_slot(0);
            let mut prior = saved(format!("{call1} {call2} {grid}").trim());
            prior.frequency_hz = frequency;
            prior.dt_seconds = shift as f32 / 12000.0;
            decoder.remember(&prior);
            decoder.begin_slot(2);
        } else {
            settings.priority_hz = Some(frequency);
            settings.ap.my_call = Some(call1.into());
            settings.ap.dx_call = Some(call2.into());
            settings.ap.dx_grid = (!grid.is_empty()).then(|| grid.into());
        }
        let found = decoder
            .decode_with_baseline(
                &audio,
                &settings,
                &mut MessageDecoder::default(),
                &AtomicBool::new(false),
                Some(&vec![1.0; 1921]),
            )
            .unwrap();
        let expected = case["message"].as_str().unwrap();
        assert_eq!(
            found.len(),
            usize::from(!expected.is_empty()),
            "{}: {:?}",
            case["id"],
            found.iter().map(|c| &c.signal).collect::<Vec<_>>()
        );
        if let Some(candidate) = found.first() {
            let d = &candidate.signal;
            assert_eq!(d.message.text, expected, "{}", case["id"]);
            assert_eq!(d.diagnostics.as_ref().unwrap().ap_type, Some(kind));
            assert!(
                (d.frequency_hz - case["frequency"].as_f64().unwrap() as f32).abs() <= 0.063,
                "{}: {d:?}",
                case["id"]
            );
            assert!(
                (d.dt_seconds - case["dt"].as_f64().unwrap() as f32).abs() <= 0.0051,
                "{}: {d:?}",
                case["id"]
            );
            let native_snr = case["snr"].as_f64().unwrap() as f32;
            let expected_snr = if kind == 7 {
                native_snr as i32
            } else {
                native_snr.round() as i32
            };
            assert!(
                (d.snr_db - expected_snr).abs() <= 1,
                "{}: {d:?}, native SNR {native_snr}",
                case["id"]
            );
            if kind == 7 {
                assert_eq!(
                    d.diagnostics.as_ref().unwrap().hard_errors,
                    case["hard_errors"].as_u64().unwrap() as usize,
                    "{}",
                    case["id"]
                );
            } else {
                assert_eq!(
                    d.diagnostics.as_ref().unwrap().questionable,
                    case["metric"].as_f64().unwrap() < -147.0,
                    "{}",
                    case["id"]
                );
            }
        }
    }
}

#[test]
fn cancellation_and_invalid_input_do_not_mutate_history() {
    let audio = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    let mut decoder = Ft8ListDecoder::new();
    decoder.begin_slot(0);
    decoder.remember(&saved("CQ K1ABC FN42"));
    decoder.begin_slot(2);
    assert!(
        decoder
            .decode(
                &audio,
                &targeted(),
                &mut MessageDecoder::default(),
                &AtomicBool::new(true)
            )
            .unwrap()
            .is_empty()
    );
    assert!(
        decoder
            .decode(
                &audio[..100],
                &targeted(),
                &mut MessageDecoder::default(),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert_eq!(decode(&mut decoder, &audio, &auto_settings()).len(), 1);
}

#[test]
fn backward_slots_and_invalid_history_do_not_create_hypotheses() {
    let audio = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    let mut decoder = Ft8ListDecoder::new();
    decoder.begin_slot(100);
    decoder.remember(&saved("CQ K1ABC FN42"));
    decoder.begin_slot(98);
    decoder.begin_slot(100);
    assert!(decode(&mut decoder, &audio, &auto_settings()).is_empty());
    for dt in [f32::MAX, f32::INFINITY, f32::NAN] {
        decoder.reset();
        decoder.begin_slot(0);
        let mut signal = saved("CQ K1ABC FN42");
        signal.dt_seconds = dt;
        decoder.remember(&signal);
        decoder.begin_slot(2);
        assert!(decode(&mut decoder, &audio, &auto_settings()).is_empty());
    }
}

#[test]
fn both_list_paths_recover_signal_missed_by_unassisted_decoder_at_all_depths() {
    let clean = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    let noise = pcm(include_bytes!("fixtures/ft8-noise.wav"));
    let audio: Vec<_> = clean
        .iter()
        .zip(&noise)
        .map(|(&c, &n)| c * 0.2 + n)
        .collect();
    for depth in 1..=3 {
        let settings = DecodeSettings {
            depth,
            assistance: false,
            ..auto_settings()
        };
        let mut found = Vec::new();
        ftdecode::decode::ft8::Ft8Decoder::new()
            .decode(
                &audio,
                &settings,
                &mut MessageDecoder::default(),
                &AtomicBool::new(false),
                &mut |d| found.push(d.message.text),
            )
            .unwrap();
        assert!(found.is_empty(), "depth {depth}: {found:?}");
        let mut list = Ft8ListDecoder::new();
        list.begin_slot(0);
        list.remember(&saved("W9XYZ K1ABC FN42"));
        list.begin_slot(2);
        let settings = DecodeSettings {
            depth,
            ..auto_settings()
        };
        let found = decode(&mut list, &audio, &settings);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].message.text, "CQ K1ABC FN42");
        let mut settings = targeted();
        settings.depth = depth;
        let found = decode(&mut Ft8ListDecoder::new(), &audio, &settings);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].message.text, "CQ K1ABC FN42");
    }
}

#[test]
fn successful_list_result_suppresses_other_history_for_same_transmitter() {
    let audio = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    let mut decoder = Ft8ListDecoder::new();
    decoder.begin_slot(0);
    decoder.remember(&saved("CQ K1ABC FN42"));
    decoder.remember(&saved("W9XYZ K1ABC FN42"));
    decoder.begin_slot(2);
    let found = decode(&mut decoder, &audio, &auto_settings());
    assert_eq!(found.len(), 1, "{found:?}");
}

#[test]
fn list_paths_reject_zero_local_evidence_in_nonzero_audio() {
    let clean = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    // Finite subnormal-scale audio has no representable demodulated power.
    let tiny = vec![f32::MIN_POSITIVE; 180000];
    for (audio, dt) in [(&clean, 15.0), (&tiny, 0.0)] {
        let mut decoder = Ft8ListDecoder::new();
        decoder.begin_slot(0);
        let mut prior = saved("W9XYZ K1ABC FN42");
        prior.dt_seconds = dt;
        decoder.remember(&prior);
        decoder.begin_slot(2);
        let found = decode(&mut decoder, audio, &auto_settings());
        assert!(
            found.is_empty(),
            "a7 at dt={dt} accepted no local evidence: {found:?}"
        );
    }
    let found = decode(&mut Ft8ListDecoder::new(), &tiny, &targeted());
    assert!(found.is_empty(), "a8 accepted no local evidence: {found:?}");
}
