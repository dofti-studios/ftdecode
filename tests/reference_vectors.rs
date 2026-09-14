// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{fec, symbols};
use serde_json::Value;

fn vectors() -> Vec<Value> {
    let data: Value = serde_json::from_str(include_str!("fixtures/message-vectors.json")).unwrap();
    data["vectors"].as_array().unwrap().clone()
}

fn bits<const N: usize>(text: &str) -> [bool; N] {
    assert!(text.bytes().all(|b| b == b'0' || b == b'1'));
    text.bytes()
        .map(|b| b == b'1')
        .collect::<Vec<_>>()
        .try_into()
        .unwrap()
}

fn codeword(vector: &Value) -> [bool; 174] {
    bits(&format!(
        "{}{}{}",
        vector["fec_input_bits"].as_str().unwrap(),
        vector["crc"].as_str().unwrap(),
        vector["parity"].as_str().unwrap()
    ))
}

#[test]
fn crc_matches_native_vectors_including_ft4_scrambling() {
    for v in vectors() {
        let message = bits(v["fec_input_bits"].as_str().unwrap());
        let expected = u16::from_str_radix(v["crc"].as_str().unwrap(), 2).unwrap();
        assert_eq!(
            fec::crc14(&message),
            expected,
            "{} {}",
            v["mode"],
            v["message"]
        );
    }
}

#[test]
fn systematic_codewords_match_native_parity() {
    for v in vectors() {
        assert_eq!(
            fec::encode(&bits(v["fec_input_bits"].as_str().unwrap())),
            codeword(&v),
            "{} {}",
            v["mode"],
            v["message"]
        );
    }
}

#[test]
fn ft4_scrambling_matches_native_fec_input() {
    for v in vectors().into_iter().filter(|v| v["mode"] == "ft4") {
        let source = bits(v["source_bits"].as_str().unwrap());
        let scrambled = bits(v["fec_input_bits"].as_str().unwrap());
        assert_eq!(symbols::scramble_ft4(&source), scrambled);
        assert_eq!(symbols::scramble_ft4(&scrambled), source);
    }
}

#[test]
fn gray_mapping_sync_and_ramps_match_native_channel_symbols() {
    for v in vectors() {
        let cw = codeword(&v);
        let actual = if v["mode"] == "ft8" {
            symbols::ft8(&cw).to_vec()
        } else {
            symbols::ft4(&cw).to_vec()
        };
        let expected: Vec<_> = v["tones"]
            .as_str()
            .unwrap()
            .bytes()
            .map(|b| b - b'0')
            .collect();
        assert_eq!(actual, expected, "{} {}", v["mode"], v["message"]);
    }
}
