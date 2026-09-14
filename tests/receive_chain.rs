// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{fec, message::MessageDecoder, symbols};
use serde_json::Value;

#[test]
fn both_modes_recover_native_messages_from_damaged_soft_codewords() {
    let data: Value = serde_json::from_str(include_str!("fixtures/message-vectors.json")).unwrap();
    for vector in data["vectors"].as_array().unwrap() {
        let native = format!(
            "{}{}{}",
            vector["fec_input_bits"].as_str().unwrap(),
            vector["crc"].as_str().unwrap(),
            vector["parity"].as_str().unwrap()
        );
        let mut llr: [f32; 174] = native
            .bytes()
            .map(|b| if b == b'1' { 6.0 } else { -6.0 })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        for index in [3, 41, 92] {
            llr[index] *= -0.2;
        }
        let decoded = fec::decode_hybrid(&llr, 3, 2).unwrap().unwrap();
        let payload = if vector["mode"] == "ft4" {
            symbols::scramble_ft4(&decoded.decoded.message)
        } else {
            decoded.decoded.message
        };
        let message = MessageDecoder::default().unpack(&payload).unwrap();
        assert_eq!(
            message.text,
            vector["message"].as_str().unwrap(),
            "{}",
            vector["mode"]
        );
    }
}

#[test]
fn both_modes_unpack_weak_messages_that_require_osd() {
    let data: Value = serde_json::from_str(include_str!("fixtures/osd-vectors.json")).unwrap();
    for (id, ft4, expected) in [
        ("ft8-2-noisy", false, "W9XYZ K1ABC -11"),
        ("ft4-weak-1", true, "CQ K1ABC FN42"),
    ] {
        let input = data["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|input| input["id"] == id)
            .unwrap();
        let llr: [f32; 174] = input["llr"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_f64().unwrap() as f32)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        assert!(fec::decode(&llr, 30).unwrap().is_none(), "{id}");
        let decoded = fec::decode_hybrid(&llr, 3, 2).unwrap().unwrap();
        assert_eq!(decoded.method, fec::DecodeMethod::Osd, "{id}");
        let payload = if ft4 {
            symbols::scramble_ft4(&decoded.decoded.message)
        } else {
            decoded.decoded.message
        };
        let message = MessageDecoder::default().unpack(&payload).unwrap();
        assert_eq!(message.text, expected, "{id}");
    }
}
