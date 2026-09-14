// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::message::{MessageDecoder, MessageKind};
use serde_json::Value;

fn reference() -> Value {
    serde_json::from_str(include_str!("fixtures/receive-message-vectors.json")).unwrap()
}
fn bits(text: &str) -> [bool; 77] {
    text.bytes()
        .map(|b| b == b'1')
        .collect::<Vec<_>>()
        .try_into()
        .unwrap()
}
fn named(name: &str) -> [bool; 77] {
    let data = reference();
    let session = data["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap();
    bits(session["messages"][0]["bits"].as_str().unwrap())
}

#[test]
fn received_messages_and_history_match_native_sessions() {
    for session in reference()["sessions"].as_array().unwrap() {
        let mut decoder = MessageDecoder::default();
        for seed in session["seeds"].as_array().unwrap() {
            decoder.remember_call(seed.as_str().unwrap()).unwrap();
        }
        for message in session["messages"].as_array().unwrap() {
            let result = decoder.unpack(&bits(message["bits"].as_str().unwrap()));
            assert_eq!(
                result.is_ok(),
                message["valid"].as_bool().unwrap(),
                "{}: {result:?}",
                session["name"]
            );
            if let Ok(result) = result {
                assert_eq!(
                    result.text,
                    message["text"].as_str().unwrap(),
                    "{}",
                    session["name"]
                );
            }
        }
    }
}

#[test]
fn all_three_hash_widths_match_native_wrapping_arithmetic() {
    let mut decoder = MessageDecoder::default();
    for h in reference()["hashes"].as_array().unwrap() {
        let expected: Vec<_> = h["hashes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect();
        assert_eq!(
            decoder
                .remember_call(h["call"].as_str().unwrap())
                .unwrap()
                .as_slice(),
            expected
        );
    }
}

#[test]
fn rr73_is_an_acknowledgement_not_a_grid() {
    let mut decoder = MessageDecoder::default();
    let grid = decoder.unpack(&named("CQ K1ABC FN42")).unwrap();
    assert_eq!(grid.grid.as_deref(), Some("FN42"));
    let rr73 = decoder.unpack(&named("K1ABC W9XYZ RR73")).unwrap();
    assert_eq!(rr73.kind, MessageKind::Standard);
    assert_eq!(rr73.grid, None);
    // Both the grid-shaped value and the explicit acknowledgement value occur.
    let mut explicit = named("K1ABC W9XYZ RR73");
    for i in 0..15 {
        explicit[59 + i] = 32403 & (1 << (14 - i)) != 0;
    }
    let decoded = decoder.unpack(&explicit).unwrap();
    assert_eq!(decoded.text, "K1ABC W9XYZ RR73");
    assert_eq!(decoded.grid, None);
}

#[test]
fn cq_nonstandard_call_does_not_claim_an_unused_hash() {
    let message = MessageDecoder::default()
        .unpack(&named("CQ KH1/KH7Z"))
        .unwrap();
    assert!(message.hashes.is_empty());
}

#[test]
fn oldest_22_bit_hash_is_evicted_when_history_is_full() {
    let mut decoder = MessageDecoder::default();
    let old = decoder.remember_call("PJ4/K1ABC").unwrap()[2];
    let mut seen = std::collections::HashSet::from([old]);
    let mut probe = MessageDecoder::default();
    for n in 0..2000 {
        let call = format!(
            "K1{}{}{}",
            (b'A' + (n / 676) as u8) as char,
            (b'A' + (n / 26 % 26) as u8) as char,
            (b'A' + (n % 26) as u8) as char
        );
        let hash = probe.remember_call(&call).unwrap()[2];
        if seen.insert(hash) {
            decoder.remember_call(&call).unwrap();
        }
        if seen.len() == 1001 {
            break;
        }
    }
    assert_eq!(seen.len(), 1001);
    let message = decoder.unpack(&named("W9XYZ <PJ4/K1ABC> -11")).unwrap();
    assert_eq!(message.hashes[0].resolved, None);
}

#[test]
fn malformed_calls_and_reserved_token_values_are_rejected() {
    let mut decoder = MessageDecoder::default();
    for call in [
        "",
        "A",
        "<...>",
        "K1 ABC",
        "K1éBC",
        "ABCDEFGHIJKLMN",
        "<K1ABC",
    ] {
        assert!(decoder.remember_call(call).is_err(), "{call}");
    }
    let mut wire = named("CQ K1ABC FN42");
    for (i, bit) in wire[..28].iter_mut().enumerate() {
        *bit = 532444 & (1 << (27 - i)) != 0;
    }
    assert!(decoder.unpack(&wire).is_err());
}

#[test]
fn malformed_bit_patterns_do_not_panic() {
    let mut decoder = MessageDecoder::default();
    let mut state = 0x1234_5678_u64;
    for _ in 0..4096 {
        let payload = std::array::from_fn(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            state >> 63 != 0
        });
        let _ = decoder.unpack(&payload);
    }
}

#[test]
fn unresolved_hashes_keep_numeric_identity_and_reset_clears_history() {
    let mut decoder = MessageDecoder::default();
    let wire = named("W9XYZ <PJ4/K1ABC> -11");
    let unresolved = decoder.unpack(&wire).unwrap();
    assert_eq!(unresolved.hashes.len(), 1);
    assert_eq!(unresolved.hashes[0].width, 22);
    assert_eq!(unresolved.hashes[0].resolved, None);
    let values = decoder.remember_call("PJ4/K1ABC").unwrap();
    assert_eq!(unresolved.hashes[0].value, values[2]);
    assert_eq!(
        decoder.unpack(&wire).unwrap().hashes[0].resolved.as_deref(),
        Some("PJ4/K1ABC")
    );
    decoder.reset();
    assert_eq!(decoder.unpack(&wire).unwrap().hashes[0].resolved, None);
}

#[test]
fn invalid_message_does_not_teach_a_callsign() {
    let mut decoder = MessageDecoder::default();
    assert!(decoder.unpack(&named("invalid grid boundary")).is_err());
    let mut target = named("W9XYZ <PJ4/K1ABC> -11");
    let hash = MessageDecoder::default().remember_call("K1ABC").unwrap()[2];
    let field = 2_063_592 + hash;
    for i in 0..28 {
        target[29 + i] = field & (1 << (27 - i)) != 0;
    }
    assert_eq!(decoder.unpack(&target).unwrap().hashes[0].resolved, None);
}
