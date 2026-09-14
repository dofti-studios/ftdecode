// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::fec;
use serde_json::Value;

fn cases() -> Vec<Value> {
    let data: Value = serde_json::from_str(include_str!("fixtures/bp-vectors.json")).unwrap();
    data["cases"].as_array().unwrap().clone()
}

#[test]
fn soft_input_decisions_codewords_and_iterations_match_native_bp() {
    for case in cases() {
        let llr: [f32; 174] = case["llr"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let actual = fec::decode(&llr, case["max_iterations"].as_u64().unwrap() as u8).unwrap();
        assert_eq!(
            actual.is_some(),
            case["accepted"].as_bool().unwrap(),
            "{}",
            case["id"]
        );
        if let Some(decoded) = actual {
            let expected: Vec<_> = case["codeword"]
                .as_str()
                .unwrap()
                .bytes()
                .map(|b| b == b'1')
                .collect();
            assert_eq!(decoded.codeword.as_slice(), expected, "{}", case["id"]);
            assert_eq!(decoded.message.as_slice(), &expected[..77]);
            assert_eq!(
                decoded.iterations as u64,
                case["iterations"].as_u64().unwrap(),
                "{}",
                case["id"]
            );
            assert_eq!(
                decoded.hard_errors as u64,
                case["hard_errors"].as_u64().unwrap(),
                "{}",
                case["id"]
            );
        }
    }
}

#[test]
fn nonfinite_likelihoods_are_errors_not_messages() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut llr = [-1.0; 174];
        llr[42] = value;
        assert_eq!(fec::decode(&llr, 30), Err(fec::InvalidLlr { index: 42 }));
    }
}

#[test]
fn absent_information_does_not_create_a_message() {
    assert_eq!(fec::decode(&[0.0; 174], 30), Ok(None));
}

#[test]
fn zero_crc_payload_is_a_valid_codeword_when_evidence_exists() {
    let decoded = fec::decode(&[-6.0; 174], 0).unwrap().unwrap();
    assert_eq!(decoded.codeword, [false; 174]);
    assert_eq!(decoded.iterations, 0);
}
