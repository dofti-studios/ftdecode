// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{
    decode::{ft8::Ft8Decoder, parallel_ft8::ParallelFt8Decoder},
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
fn settings(threads: usize) -> DecodeSettings {
    DecodeSettings {
        threads,
        low_hz: 1450.0,
        high_hz: 1550.0,
        assistance: false,
        depth: 1,
        ..DecodeSettings::default()
    }
}
fn identity(signals: &[DecodedSignal]) -> Vec<([bool; 77], f32, f32, i32)> {
    signals
        .iter()
        .map(|d| (d.source_bits, d.frequency_hz, d.dt_seconds, d.snr_db))
        .collect()
}

#[test]
fn one_thread_preserves_sequential_results_and_statistics() {
    let audio = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    let config = settings(1);
    let mut sequential = Vec::new();
    let reference = Ft8Decoder::new()
        .decode(
            &audio,
            &config,
            &mut MessageDecoder::default(),
            &AtomicBool::new(false),
            &mut |d| sequential.push(d),
        )
        .unwrap();
    let mut parallel = Vec::new();
    let actual = ParallelFt8Decoder::new()
        .decode(
            &audio,
            &config,
            &mut MessageDecoder::default(),
            &AtomicBool::new(false),
            &mut |d| parallel.push(d),
            None,
        )
        .unwrap();
    assert_eq!(identity(&parallel), identity(&sequential));
    assert_eq!(actual.candidates, reference.candidates);
    assert_eq!(actual.passes, reference.passes);
    assert_eq!(actual.decoded, reference.decoded);
}

#[test]
fn band_boundary_and_guard_overlap_emit_signal_once() {
    let audio = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    for threads in [2, 4, 12] {
        let mut decoder = ParallelFt8Decoder::new();
        let mut prior = Vec::new();
        for slot in [100, 102] {
            let mut found = Vec::new();
            let stats = decoder
                .decode(
                    &audio,
                    &settings(threads),
                    &mut MessageDecoder::default(),
                    &AtomicBool::new(false),
                    &mut |d| found.push(d),
                    Some(slot),
                )
                .unwrap();
            assert_eq!(found.len(), 1, "threads {threads}: {found:?}");
            assert_eq!(found[0].message.text, "CQ K1ABC FN42");
            assert!((found[0].frequency_hz - 1500.0).abs() < 1.0);
            assert_eq!(stats.decoded, 1);
            if slot == 102 {
                assert_eq!(identity(&found), prior);
            }
            prior = identity(&found);
        }
    }
}

#[test]
fn guard_signals_are_subtracted_to_recover_overlapping_owned_signal() {
    let audio = pcm(include_bytes!("fixtures/ft8-mixture.wav"));
    let mut found = Vec::new();
    let config = DecodeSettings {
        depth: 3,
        ..settings(4)
    };
    let stats = ParallelFt8Decoder::new()
        .decode(
            &audio,
            &config,
            &mut MessageDecoder::default(),
            &AtomicBool::new(false),
            &mut |d| found.push(d),
            None,
        )
        .unwrap();
    assert_eq!(found.len(), 2, "{found:?}");
    for text in ["CQ K1ABC FN42", "K1ABC W9XYZ EN37"] {
        assert!(found.iter().any(|d| d.message.text == text), "{found:?}");
    }
    assert_eq!(stats.decoded, 2);
}

#[test]
fn pre_cancelled_workers_do_not_search_or_emit() {
    let mut found = Vec::new();
    let stats = ParallelFt8Decoder::new()
        .decode(
            &vec![0.0; 180000],
            &settings(4),
            &mut MessageDecoder::default(),
            &AtomicBool::new(true),
            &mut |d| found.push(d),
            None,
        )
        .unwrap();
    assert!(stats.cancelled);
    assert_eq!(stats.candidates, 0);
    assert_eq!(stats.passes, 0);
    assert!(found.is_empty());
}

#[test]
fn callback_cancellation_stops_buffered_delivery() {
    let audio = pcm(include_bytes!("fixtures/ft8-mixture.wav"));
    let cancel = AtomicBool::new(false);
    let mut count = 0;
    let stats = ParallelFt8Decoder::new()
        .decode(
            &audio,
            &DecodeSettings {
                depth: 3,
                ..settings(4)
            },
            &mut MessageDecoder::default(),
            &cancel,
            &mut |_| {
                count += 1;
                cancel.store(true, Ordering::Relaxed);
            },
            None,
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(stats.decoded, 1);
    assert!(stats.cancelled);
}

#[test]
fn invalid_threads_and_audio_are_rejected_even_when_cancelled() {
    let mut decoder = ParallelFt8Decoder::new();
    for threads in [0, 13, usize::MAX] {
        assert!(
            decoder
                .decode(
                    &vec![0.0; 180000],
                    &settings(threads),
                    &mut MessageDecoder::default(),
                    &AtomicBool::new(true),
                    &mut |_| {},
                    None
                )
                .is_err()
        );
    }
    for audio in [
        vec![0.0; 179999],
        vec![f32::NAN; 180000],
        vec![32769.0; 180000],
    ] {
        assert!(
            decoder
                .decode(
                    &audio,
                    &settings(2),
                    &mut MessageDecoder::default(),
                    &AtomicBool::new(true),
                    &mut |_| {},
                    None
                )
                .is_err()
        );
    }
}
