// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{
    assistance::ApMode,
    decode::{ft8::Ft8Decoder, parallel_ft8::ParallelFt8Decoder},
    engine::{DecodeSettings, DecodedSignal, Decoder, Mode},
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

fn history_audio() -> (Vec<f32>, Vec<f32>) {
    let clean = pcm(include_bytes!("fixtures/ft8-clean.wav"));
    let noise = pcm(include_bytes!("fixtures/ft8-noise.wav"));
    let weak = clean.iter().zip(noise).map(|(c, n)| c * 0.2 + n).collect();
    (clean, weak)
}

fn history_settings(threads: usize) -> DecodeSettings {
    let mut settings = DecodeSettings {
        depth: 1,
        threads,
        assistance: true,
        ..DecodeSettings::default()
    };
    settings.ap.mode = ApMode::Auto;
    settings
}

fn assert_a7(found: &[DecodedSignal]) {
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].message.text, "CQ K1ABC FN42");
    assert_eq!(found[0].diagnostics.as_ref().unwrap().ap_type, Some(7));
}

#[test]
fn engine_preserves_a7_history_across_worker_changes() {
    let (clean, weak) = history_audio();
    let cancel = AtomicBool::new(false);
    for (before, after, high_hz) in [
        (1, 1, 3000.0),
        (2, 2, 3000.0),
        (1, 2, 3000.0),
        (2, 1, 3000.0),
        (2, 4, 3000.0),
        (4, 2, 3000.0),
        (12, 1, 3000.0),
        (2, 2, 2400.0), // Repartition without changing worker count.
    ] {
        let mut decoder = Decoder::new(Mode::Ft8);
        let mut config = history_settings(before);
        decoder.set_slot(0);
        let mut initial = Vec::new();
        decoder
            .decode(&clean, &config, &cancel, &mut |s| initial.push(s))
            .unwrap();
        assert_eq!(initial.len(), 1);

        config.threads = after;
        config.high_hz = high_hz;
        decoder.set_slot(2);
        let mut found = Vec::new();
        decoder
            .decode(&weak, &config, &cancel, &mut |s| found.push(s))
            .unwrap();
        assert_a7(&found);

        // The assisted decode must teach the next same-parity slot, including
        // when the caller switches back to its original worker count.
        config.threads = before;
        config.high_hz = 3000.0;
        decoder.set_slot(4);
        found.clear();
        decoder
            .decode(&weak, &config, &cancel, &mut |s| found.push(s))
            .unwrap();
        assert_a7(&found);
    }
}

#[test]
fn parallel_worker_changes_preserve_implicit_slot_history() {
    let (clean, weak) = history_audio();
    let silence = vec![0.0; 180000];
    let mut decoder = ParallelFt8Decoder::new();
    let mut messages = MessageDecoder::default();
    let cancel = AtomicBool::new(false);
    // Newly created workers must join the stream's timeline, not start at 0.
    for (slot, threads) in [1, 2, 4, 2, 1].into_iter().enumerate() {
        let audio = if slot == 0 {
            &clean
        } else if slot % 2 == 0 {
            &weak
        } else {
            &silence
        };
        let mut found = Vec::new();
        decoder
            .decode(
                audio,
                &history_settings(threads),
                &mut messages,
                &cancel,
                &mut |s| found.push(s),
                None,
            )
            .unwrap();
        if slot == 0 {
            assert_eq!(found.len(), 1);
        } else if slot % 2 == 0 {
            assert_a7(&found);
        } else {
            assert!(found.is_empty());
        }
    }
}

#[test]
fn worker_changes_keep_slot_expiry_and_reset_semantics() {
    let (clean, weak) = history_audio();
    let cancel = AtomicBool::new(false);
    for (reset, next_slots) in [
        (true, &[102][..]),
        (false, &[104][..]),
        (false, &[98, 100][..]),
    ] {
        let mut decoder = Decoder::new(Mode::Ft8);
        decoder.set_slot(100);
        decoder
            .decode(&clean, &history_settings(2), &cancel, &mut |_| {})
            .unwrap();
        if reset {
            decoder.reset(Mode::Ft8);
        }
        for &slot in next_slots {
            decoder.set_slot(slot);
            let mut found = Vec::new();
            decoder
                .decode(&weak, &history_settings(1), &cancel, &mut |s| found.push(s))
                .unwrap();
            assert!(found.is_empty(), "reset={reset}, slot={slot}: {found:?}");
        }
    }
}

#[test]
fn repeated_and_cancelled_attempts_do_not_advance_shared_history() {
    let (clean, weak) = history_audio();
    let mut decoder = ParallelFt8Decoder::new();
    let mut messages = MessageDecoder::default();
    let cancel = AtomicBool::new(false);
    for threads in [1, 2] {
        decoder
            .decode(
                &clean,
                &history_settings(threads),
                &mut messages,
                &cancel,
                &mut |_| {},
                Some(100),
            )
            .unwrap();
    }
    // A rejected input and a pre-cancelled attempt must not expire history or
    // move the implicit timeline to the supplied far-future slot.
    assert!(
        decoder
            .decode(
                &weak[..100],
                &history_settings(4),
                &mut messages,
                &cancel,
                &mut |_| panic!("invalid input emitted a signal"),
                Some(200),
            )
            .is_err()
    );
    let stats = decoder
        .decode(
            &weak,
            &history_settings(4),
            &mut messages,
            &AtomicBool::new(true),
            &mut |_| panic!("cancelled input emitted a signal"),
            Some(200),
        )
        .unwrap();
    assert!(stats.cancelled);
    // AP off still advances a real opposite-parity slot, retaining history.
    let mut config = history_settings(4);
    config.assistance = false;
    decoder
        .decode(
            &vec![0.0; 180000],
            &config,
            &mut messages,
            &cancel,
            &mut |_| {},
            None,
        )
        .unwrap();
    let mut found = Vec::new();
    decoder
        .decode(
            &weak,
            &history_settings(1),
            &mut messages,
            &cancel,
            &mut |s| found.push(s),
            None,
        )
        .unwrap();
    assert_a7(&found);
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
