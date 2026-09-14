// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{
    engine::{DecodedSignal, Mode},
    message::MessageDecoder,
    stream::signal_json,
};
use serde_json::{Value, json};

fn event(decoder: &mut MessageDecoder, bits: [bool; 77]) -> Value {
    let signal = DecodedSignal {
        message: decoder.unpack(&bits).unwrap(),
        source_bits: bits,
        frequency_hz: 1500.0,
        snr_db: -23,
        dt_seconds: 0.1,
        assisted: false,
        diagnostics: None,
    };
    signal_json(&signal, Mode::Ft8, 0, None)
}

fn named(name: &str) -> Value {
    let data: Value =
        serde_json::from_str(include_str!("fixtures/receive-message-vectors.json")).unwrap();
    let session = data["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap();
    let mut decoder = MessageDecoder::default();
    for seed in session["seeds"].as_array().unwrap() {
        decoder.remember_call(seed.as_str().unwrap()).unwrap();
    }
    let bits = session["messages"][0]["bits"]
        .as_str()
        .unwrap()
        .bytes()
        .map(|b| b == b'1')
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    event(&mut decoder, bits)
}

#[test]
fn standard_exchange_metadata_distinguishes_sequence_steps_and_report_from_snr() {
    for (text, exchange, sender, recipient, grid, report, modifier) in [
        (
            "CQ K1ABC FN42",
            "cq",
            "K1ABC",
            None,
            Some("FN42"),
            None,
            None,
        ),
        (
            "CQ FD K1ABC FN42",
            "cq",
            "K1ABC",
            None,
            Some("FN42"),
            None,
            Some("FD"),
        ),
        (
            "CQ 000 K1ABC FN42",
            "cq",
            "K1ABC",
            None,
            Some("FN42"),
            None,
            Some("000"),
        ),
        (
            "QRZ K1ABC AA00",
            "qrz",
            "K1ABC",
            None,
            Some("AA00"),
            None,
            None,
        ),
        (
            "DE K1ABC FN42",
            "de",
            "K1ABC",
            None,
            Some("FN42"),
            None,
            None,
        ),
        (
            "K1ABC W9XYZ EN37",
            "grid",
            "W9XYZ",
            Some("K1ABC"),
            Some("EN37"),
            None,
            None,
        ),
        (
            "W9XYZ K1ABC/R R FN42",
            "roger_grid",
            "K1ABC/R",
            Some("W9XYZ"),
            Some("FN42"),
            None,
            None,
        ),
        (
            "W9XYZ K1ABC -11",
            "report",
            "K1ABC",
            Some("W9XYZ"),
            None,
            Some(-11),
            None,
        ),
        (
            "K1ABC W9XYZ R-09",
            "roger_report",
            "W9XYZ",
            Some("K1ABC"),
            None,
            Some(-9),
            None,
        ),
        (
            "K1ABC W9XYZ -50",
            "report",
            "W9XYZ",
            Some("K1ABC"),
            None,
            Some(-50),
            None,
        ),
        (
            "K1ABC W9XYZ +49",
            "report",
            "W9XYZ",
            Some("K1ABC"),
            None,
            Some(49),
            None,
        ),
        (
            "W9XYZ K1ABC RRR",
            "rrr",
            "K1ABC",
            Some("W9XYZ"),
            None,
            None,
            None,
        ),
        (
            "K1ABC W9XYZ RR73",
            "rr73",
            "W9XYZ",
            Some("K1ABC"),
            None,
            None,
            None,
        ),
        (
            "K1ABC W9XYZ 73",
            "73",
            "W9XYZ",
            Some("K1ABC"),
            None,
            None,
            None,
        ),
        (
            "K1ABC W9XYZ",
            "calls",
            "W9XYZ",
            Some("K1ABC"),
            None,
            None,
            None,
        ),
        (
            "CQ G4ABC/P IO91",
            "cq",
            "G4ABC/P",
            None,
            Some("IO91"),
            None,
            None,
        ),
    ] {
        let e = named(text);
        assert_eq!(e["message_type"], "standard", "{text}");
        assert_eq!(e["exchange_type"], exchange, "{text}");
        assert_eq!(e["sender"], sender, "{text}");
        assert_eq!(e["recipient"], json!(recipient), "{text}");
        assert_eq!(e["grid"], json!(grid), "{text}");
        assert_eq!(e["report_db"], json!(report), "{text}");
        assert_eq!(e["cq_modifier"], json!(modifier), "{text}");
        assert_eq!(e["snr_db"], -23);
        for field in ["sender_hash", "recipient_hash"] {
            assert_eq!(e.get(field), Some(&Value::Null), "{text}: {field}");
        }
    }
}

#[test]
fn hashed_identities_are_associated_with_the_correct_role() {
    for (text, role, other_role, other_call, width, exchange) in [
        (
            "W9XYZ <PJ4/K1ABC> -11",
            "sender",
            "recipient",
            "W9XYZ",
            22,
            "report",
        ),
        (
            "<PJ4/K1ABC> W9XYZ R-09",
            "recipient",
            "sender",
            "W9XYZ",
            22,
            "roger_report",
        ),
        (
            "PJ4/K1ABC <W9XYZ>",
            "sender",
            "recipient",
            "PJ4/K1ABC",
            12,
            "calls",
        ),
        (
            "<W9XYZ> PJ4/K1ABC RRR",
            "recipient",
            "sender",
            "PJ4/K1ABC",
            12,
            "rrr",
        ),
        (
            "<KA1ABC> YW18FIFA RR73",
            "recipient",
            "sender",
            "YW18FIFA",
            12,
            "rr73",
        ),
    ] {
        let e = named(text);
        let hash_field = format!("{role}_hash");
        assert_eq!(e["exchange_type"], exchange, "{text}");
        assert_eq!(e.get(role), Some(&Value::Null), "{text}");
        assert_eq!(e[other_role], other_call, "{text}");
        assert_eq!(e[&hash_field], e["hashes"][0], "{text}");
        assert_eq!(e[&hash_field]["width"], width);
        assert!(e[&hash_field]["value"].is_u64());
        let known = named(&format!("{text} (known hashes)"));
        assert_eq!(known[&hash_field]["value"], e[&hash_field]["value"]);
        assert_eq!(known[role], known[&hash_field]["resolved"]);
        assert!(
            known[role]
                .as_str()
                .is_some_and(|s| !s.contains(['<', '>']))
        );
    }
    let cq = named("CQ PJ4/K1ABC");
    assert_eq!(cq["message_type"], "nonstandard");
    assert_eq!(cq["exchange_type"], "cq");
    assert_eq!(cq["sender"], "PJ4/K1ABC");
    assert!(cq["recipient_hash"].is_null());
}

#[test]
fn other_formats_remain_unclassified_for_standard_sequencing() {
    for (text, kind) in [
        ("TNX BOB 73 GL", "free_text"),
        ("123456789ABCDEF012", "telemetry"),
        ("K1ABC RR73; W9XYZ <KH1/KH7Z> -08", "dxpedition"),
        ("W9XYZ K1ABC R 17B EMA", "field_day"),
        ("TU; KA0DEF K1ABC R 569 MA", "rtty"),
        ("<PA9XYZ> <G4ABC/P> 570123 IO91NP", "vhf"),
        ("K1ABC FN42 37", "wspr"),
    ] {
        let e = named(text);
        assert_eq!(e["message_type"], kind, "{text}");
        assert_eq!(e.get("exchange_type"), Some(&Value::Null), "{text}");
        assert_eq!(e.get("report_db"), Some(&Value::Null), "{text}");
    }
    let vhf = named("<PA9XYZ> <G4ABC/P> 570123 IO91NP");
    assert_eq!(vhf["recipient_hash"]["width"], 12);
    assert_eq!(vhf["sender_hash"]["width"], 22);
    assert_eq!(vhf["grid"], "IO91NP");
    let contest = named("TU; KA0DEF K1ABC R 569 MA");
    assert_eq!(contest["sender"], "K1ABC");
    assert_eq!(contest["recipient"], "KA0DEF");
}

#[test]
fn free_text_that_looks_like_cq_is_not_a_structured_call() {
    let alphabet = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-./?";
    let packed = b"CQ K1ABC     ".iter().fold(0_u128, |n, c| {
        n * 42 + alphabet.iter().position(|x| x == c).unwrap() as u128
    });
    let mut bits = [false; 77];
    for (i, b) in bits[..71].iter_mut().enumerate() {
        *b = packed & (1 << (70 - i)) != 0;
    }
    let e = event(&mut MessageDecoder::default(), bits);
    assert_eq!(e["message"], "CQ K1ABC");
    assert_eq!(e["message_type"], "free_text");
    for field in [
        "exchange_type",
        "sender",
        "recipient",
        "grid",
        "report_db",
        "sender_hash",
        "recipient_hash",
        "cq_modifier",
    ] {
        assert_eq!(e.get(field), Some(&Value::Null), "{field}");
    }
}

#[test]
fn both_rr73_encodings_are_acknowledgements_but_r_grid_rr73_is_unclassified() {
    let bits = ftdecode::message_encode::encode("K1ABC W9XYZ RR73").unwrap();
    for encoded in [32403, 32373] {
        // 32373 is the locator-shaped RR73 value; 32403 is explicit RR73.
        let mut bits = bits;
        for i in 0..15 {
            bits[59 + i] = encoded & (1 << (14 - i)) != 0;
        }
        let e = event(&mut MessageDecoder::default(), bits);
        assert_eq!(e["message"], "K1ABC W9XYZ RR73");
        assert_eq!(e["exchange_type"], "rr73");
        assert!(e["grid"].is_null());
        if encoded == 32373 {
            bits[58] = true;
            let e = event(&mut MessageDecoder::default(), bits);
            assert_eq!(e["message"], "K1ABC W9XYZ R RR73");
            assert!(e["exchange_type"].is_null());
        }
    }
}

#[test]
fn role_hash_resolution_uses_current_history_and_reset_clears_it() {
    let bits = ftdecode::message_encode::encode("<K1ABC> <W9XYZ> R-12").unwrap();
    let mut decoder = MessageDecoder::default();
    let unknown = event(&mut decoder, bits);
    assert_eq!(unknown["exchange_type"], "roger_report");
    assert_ne!(
        unknown["sender_hash"]["value"],
        unknown["recipient_hash"]["value"]
    );
    decoder.remember_call("K1ABC").unwrap();
    decoder.remember_call("W9XYZ").unwrap();
    let known = event(&mut decoder, bits);
    assert_eq!(known["sender"], "W9XYZ");
    assert_eq!(known["recipient"], "K1ABC");
    for role in ["sender", "recipient"] {
        let hash = format!("{role}_hash");
        assert_eq!(unknown[&hash]["value"], known[&hash]["value"]);
        assert!(unknown[role].is_null());
    }
    decoder.reset();
    assert_eq!(event(&mut decoder, bits), unknown);
}

#[test]
fn address_token_suffix_bits_do_not_create_callsign_recipients() {
    for (text, exchange, modifier) in [
        ("QRZ K1ABC AA00", "qrz", None),
        ("CQ FD K1ABC FN42", "cq", Some("FD")),
    ] {
        for portable in [false, true] {
            let mut bits = ftdecode::message_encode::encode(text).unwrap();
            bits[28] = true;
            bits[75] = portable;
            bits[76] = !portable;
            let e = event(&mut MessageDecoder::default(), bits);
            assert_eq!(e["exchange_type"], exchange);
            assert_eq!(e.get("recipient"), Some(&Value::Null), "{e}");
            assert_eq!(e.get("recipient_hash"), Some(&Value::Null));
            assert_eq!(e["cq_modifier"], json!(modifier));
        }
    }
}
