use ftdecode::{decode::ft8::Ft8Decoder, engine::DecodeSettings, message::MessageDecoder};
use std::sync::atomic::{AtomicBool, Ordering};

fn clean_pcm() -> Vec<f32> {
    include_bytes!("fixtures/ft8-clean.wav")[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])))
        .collect()
}

#[test]
fn staged_decode_requires_valid_complete_audio_before_any_callback() {
    let mut audio = clean_pcm();
    let mut decoder = Ft8Decoder::new();
    let mut calls = 0;
    for bad_tail in [f32::NAN, f32::INFINITY, 32769.0] {
        audio[179999] = bad_tail;
        assert!(
            decoder
                .decode(
                    &audio,
                    &DecodeSettings::default(),
                    &mut MessageDecoder::default(),
                    &AtomicBool::new(false),
                    &mut |_| calls += 1
                )
                .is_err()
        );
    }
    assert!(
        decoder
            .decode(
                &audio[..179999],
                &DecodeSettings::default(),
                &mut MessageDecoder::default(),
                &AtomicBool::new(false),
                &mut |_| calls += 1
            )
            .is_err()
    );
    assert_eq!(calls, 0);
}

#[test]
fn staged_deduplication_is_local_to_each_slot_and_cancellation_stops_replay() {
    let audio = clean_pcm();
    let mut decoder = Ft8Decoder::new();
    let mut messages = MessageDecoder::default();
    let settings = DecodeSettings {
        assistance: false,
        ..Default::default()
    };
    // Reuse all decoder caches across cancellation and subsequent slots.
    for stop_after_first in [true, false, false] {
        let cancel = AtomicBool::new(false);
        let mut found = Vec::new();
        let stats = decoder
            .decode(&audio, &settings, &mut messages, &cancel, &mut |d| {
                found.push(d);
                if stop_after_first {
                    cancel.store(true, Ordering::Relaxed);
                }
            })
            .unwrap();
        assert_eq!(found.len(), 1, "one callback across acquisition stages");
        assert_eq!(found[0].message.text, "CQ K1ABC FN42");
        assert_eq!(stats.decoded, 1);
        assert_eq!(stats.cancelled, stop_after_first);
        if stop_after_first {
            assert_eq!(stats.passes, 1);
        }
    }
}
