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
fn received_messages_match_native_except_ambiguous_hashes() {
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
                // Native last-call-wins behavior is deliberately replaced by
                // an unresolved identity when a collision has been observed.
                let expected = if session["name"] == "latest call replaces colliding short hash" {
                    assert!(result.hashes[0].ambiguous);
                    "K1ABC RR73; W9XYZ <...> -08"
                } else {
                    message["text"].as_str().unwrap()
                };
                assert_eq!(result.text, expected, "{}", session["name"]);
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

#[test]
fn full_type4_destination_teaches_all_hash_widths() {
    use ftdecode::message_encode::encode;
    let mut decoder = MessageDecoder::default();
    decoder
        .unpack(&encode("PJ4/K1ABC <W9XYZ> RR73").unwrap())
        .unwrap();
    for (text, width) in [
        ("K1ABC RR73; W9XYZ <PJ4/K1ABC> -10", 10),
        ("<PJ4/K1ABC> YW18FIFA RR73", 12),
        ("W9XYZ <PJ4/K1ABC> -11", 22),
    ] {
        let message = decoder.unpack(&encode(text).unwrap()).unwrap();
        assert_eq!(message.hashes[0].width, width);
        assert_eq!(message.hashes[0].resolved.as_deref(), Some("PJ4/K1ABC"));
    }
    decoder.reset();
    let message = decoder
        .unpack(&encode("W9XYZ <PJ4/K1ABC> -11").unwrap())
        .unwrap();
    assert_eq!(message.hashes[0].resolved, None);
}

#[test]
fn hash_collisions_stay_unresolved_until_reset() {
    use ftdecode::message_encode::encode;
    for (a, b, width, value, text) in [
        (
            "K1AAM/P",
            "K1AQC/P",
            10,
            262,
            "K1ABC RR73; W9XYZ <K1AAM/P> -10",
        ),
        ("K1AAA/P", "K1BAP/P", 12, 364, "<K1AAA/P> PJ4/K1ABC RR73"),
        (
            "TA7CNW/P",
            "PA0UCS/P",
            22,
            2_662_519,
            "W9XYZ <TA7CNW/P> -11",
        ),
    ] {
        let bits = encode(text).unwrap();
        let mut decoder = MessageDecoder::default();
        let unknown = decoder.unpack(&bits).unwrap();
        assert_eq!(unknown.hashes[0].resolved, None);
        assert!(!unknown.hashes[0].ambiguous);
        decoder.remember_call(a).unwrap();
        decoder
            .remember_call(&format!("<{}>", a.to_ascii_lowercase()))
            .unwrap();
        let known = decoder.unpack(&bits).unwrap();
        assert_eq!(known.hashes[0].resolved.as_deref(), Some(a));
        assert!(!known.hashes[0].ambiguous);
        decoder.remember_call(b).unwrap();
        // Repeated seeds must not silently turn a collision back into a guess.
        decoder.remember_call(a).unwrap();
        for mut d in [decoder.clone(), decoder] {
            let ambiguous = d.unpack(&bits).unwrap();
            assert_eq!(ambiguous.hashes[0].width, width);
            assert_eq!(ambiguous.hashes[0].value, value);
            assert_eq!(ambiguous.hashes[0].resolved, None);
            assert!(ambiguous.hashes[0].ambiguous);
            assert!(ambiguous.text.contains("<...>"));
            if let Some(station) = ambiguous
                .fields
                .sender
                .as_ref()
                .filter(|s| s.hash.is_some())
            {
                assert_eq!(station.call, None);
                assert!(station.hash.as_ref().unwrap().ambiguous);
            }
            d.reset();
            d.remember_call(a).unwrap();
            let known = d.unpack(&bits).unwrap();
            assert!(!known.hashes[0].ambiguous);
            assert_eq!(known.hashes[0].resolved.as_deref(), Some(a));
        }
    }
}

#[test]
fn full_call_revealing_collision_invalidates_same_message_resolution() {
    let mut decoder = MessageDecoder::default();
    decoder.remember_call("K1AAA/P").unwrap();
    let wire = ftdecode::message_encode::encode("<K1AAA/P> K1BAP/P RR73").unwrap();
    let message = decoder.unpack(&wire).unwrap();
    assert_eq!(message.text, "<...> K1BAP/P RR73");
    assert!(message.hashes[0].ambiguous);
    assert_eq!(message.fields.recipient.unwrap().call, None);

    // A 12-bit collision need not prevent use of the longer, unique hash.
    let wire = ftdecode::message_encode::encode("W9XYZ <K1AAA/P> -11").unwrap();
    let message = decoder.unpack(&wire).unwrap();
    assert_eq!(message.hashes[0].resolved.as_deref(), Some("K1AAA/P"));
    assert!(!message.hashes[0].ambiguous);
}

#[test]
fn invalid_type4_does_not_teach_destination() {
    let mut decoder = MessageDecoder::default();
    let mut wire = ftdecode::message_encode::encode("PJ4/K1ABC <W9XYZ> 73").unwrap();
    wire[73] = true; // A flipped type-4 CQ would name a hashed sender.
    assert!(decoder.unpack(&wire).is_err());
    let wire = ftdecode::message_encode::encode("W9XYZ <PJ4/K1ABC> -11").unwrap();
    assert_eq!(decoder.unpack(&wire).unwrap().hashes[0].resolved, None);
}
