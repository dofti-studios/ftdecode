// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{message::MessageDecoder, message_encode::encode};

#[test]
fn matches_native_packed_bits_and_receive_text() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/message-encode-vectors.json")).unwrap();
    for vector in fixture["vectors"].as_array().unwrap() {
        let text = vector["text"].as_str().unwrap();
        let bits = encode(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        let actual: String = bits.iter().map(|&b| if b { '1' } else { '0' }).collect();
        assert_eq!(actual, vector["bits"].as_str().unwrap(), "{text}");
        let mut decoder = MessageDecoder::default();
        for seed in vector["seeds"].as_array().unwrap() {
            decoder.remember_call(seed.as_str().unwrap()).unwrap();
        }
        assert_eq!(
            decoder.unpack(&bits).unwrap().text,
            vector["decoded"].as_str().unwrap(),
            "{text}"
        );
    }
}

#[test]
fn rejects_unsupported_fields_and_out_of_range_inputs() {
    for text in [
        "",
        "HELLO WORLD",
        "0123456789ABC",
        "K1ABC FN42 37",
        "CQ <PJ4/K1ABC> FN42",
        "CQ K1ABC R FN42",
        "CQ K1ABC RR73",
        "K1ABC W9XYZ -51",
        "K1ABC W9XYZ +51",
        "K1ABC W9XYZ R",
        "K1ABC W9XYZ RRR EXTRA",
        "K1ABC W9XYZ FN42AA",
        "K1ABC W9XYZ 0A WI",
        "K1ABC W9XYZ 33A WI",
        "K1ABC W9XYZ 1I WI",
        "K1ABC W9XYZ 599 8000",
        "<PA9XYZ> <G4ABC/P> 592048 RR99XX",
        "<...> W9XYZ RRR",
        "<ABCDEFGHIJKL> W9XYZ RRR",
        "CQ ABCDEFGHIJKL",
        "K1ABC W9XYZ/R/P FN42",
        "CQ K1ABC ☃",
        "K1ABC\0 W9XYZ RRR",
        "K1ABC W9XYZ +1,2",
        "<K1ABC W9XYZ RRR",
    ] {
        assert!(encode(text).is_err(), "accepted {text:?}");
    }
    assert!(encode(&"A".repeat(1_000_000)).is_err());
}

#[test]
fn dxpedition_reports_preserve_native_two_db_resolution() {
    let mut decoder = MessageDecoder::default();
    decoder.remember_call("KH1/KH7Z").unwrap();
    for (input, expected) in [
        ("-30", "-30"),
        ("-29", "-30"),
        ("-12", "-12"),
        ("-11", "-12"),
        ("-01", "-02"),
        ("+00", "+00"),
        ("+01", "+00"),
        ("+11", "+10"),
        ("+31", "+30"),
        ("+32", "+32"),
    ] {
        let text = format!("K1ABC RR73; W9XYZ <KH1/KH7Z> {input}");
        let bits = encode(&text).unwrap();
        assert_eq!(
            decoder.unpack(&bits).unwrap().text,
            format!("K1ABC RR73; W9XYZ <KH1/KH7Z> {expected}"),
            "{text}",
        );
    }
    for report in ["-31", "+33"] {
        assert!(encode(&format!("K1ABC RR73; W9XYZ <KH1/KH7Z> {report}")).is_err());
    }
}
