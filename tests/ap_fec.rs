// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{fec, osd};

fn native_vectors() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/ap-fec-vectors.json")).unwrap()
}

fn word(bits: &[bool; 174]) -> String {
    bits.iter()
        .map(|&bit| if bit { '1' } else { '0' })
        .collect()
}

use ftdecode::{fec::decode_hybrid_ap, osd::decode_ap};

#[test]
fn hybrid_frozen_posteriors_match_native_with_correct_and_wrong_hints() {
    check_native(false);
}

#[test]
fn osd_masked_trial_flips_match_native_after_reliability_reordering() {
    check_native(true);
}

fn check_native(standalone: bool) {
    let vectors = native_vectors();
    let mut masked_successes = 0;
    let mut masked_failures = 0;
    for case in vectors["cases"].as_array().unwrap() {
        let max_osd = case["max_osd"].as_i64().unwrap();
        if (max_osd == -2) != standalone {
            continue;
        }
        let input = vectors["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|input| input["id"] == case["id"])
            .unwrap();
        let llr = std::array::from_fn(|i| input["llr"][i].as_f64().unwrap() as f32);
        let mask = std::array::from_fn(|i| input["mask"][i].as_bool().unwrap());
        let depth = case["depth"].as_u64().unwrap() as u8;
        let actual = if standalone {
            decode_ap(&llr, &mask, depth)
                .unwrap()
                .map(|result| (result.codeword, result.hard_errors, result.distance, 2))
        } else {
            decode_hybrid_ap(&llr, &mask, max_osd as u8, depth)
                .unwrap()
                .map(|result| {
                    (
                        result.decoded.codeword,
                        result.decoded.hard_errors,
                        result.distance,
                        if result.method == fec::DecodeMethod::Bp {
                            1
                        } else {
                            2
                        },
                    )
                })
        };
        let label = format!("{} max {max_osd} depth {depth}", case["id"]);
        // Public APIs deliberately reject absent evidence, unlike native BP.
        let accepted = case["method"] != 0 && llr.iter().any(|&value| value != 0.0);
        assert_eq!(actual.is_some(), accepted, "{label}");
        if mask.iter().any(|&bit| bit) {
            if accepted {
                masked_successes += 1;
            } else {
                masked_failures += 1;
            }
        }
        if let Some((codeword, hard_errors, distance, method)) = actual {
            assert_eq!(
                word(&codeword),
                case["codeword"].as_str().unwrap(),
                "{label}"
            );
            assert_eq!(
                hard_errors as i64,
                case["hard_errors"].as_i64().unwrap(),
                "{label}"
            );
            assert_eq!(method, case["method"].as_i64().unwrap(), "{label}");
            let expected = case["distance"].as_f64().unwrap() as f32;
            assert!(
                (distance - expected).abs() <= 0.00002 * expected.max(1.0),
                "{label}: distance {distance} != {expected}"
            );
        }
    }
    assert!(masked_successes > 0 && masked_failures > 0);
}

#[test]
fn zero_masks_preserve_existing_decoding_exactly() {
    let vectors = native_vectors();
    for input in vectors["inputs"].as_array().unwrap() {
        let llr = std::array::from_fn(|i| input["llr"][i].as_f64().unwrap() as f32);
        for depth in 0..=2 {
            assert_eq!(
                decode_ap(&llr, &[false; 174], depth),
                osd::decode(&llr, depth)
            );
            for max_osd in 0..=3 {
                assert_eq!(
                    decode_hybrid_ap(&llr, &[false; 174], max_osd, depth),
                    fec::decode_hybrid(&llr, max_osd, depth)
                );
            }
        }
    }
}

#[test]
fn masked_decoders_reject_invalid_inputs_and_absent_evidence() {
    let mask = [true; 174];
    for depth in 0..=2 {
        assert!(decode_ap(&[0.0; 174], &mask, depth).unwrap().is_none());
        for max_osd in 0..=3 {
            assert!(
                decode_hybrid_ap(&[0.0; 174], &mask, max_osd, depth)
                    .unwrap()
                    .is_none()
            );
        }
    }
    assert_eq!(
        decode_ap(&[1.0; 174], &mask, 3),
        Err(osd::DecodeError::UnsupportedDepth(3))
    );
    assert_eq!(
        decode_hybrid_ap(&[1.0; 174], &mask, 4, 2),
        Err(osd::DecodeError::UnsupportedMaxOsd(4))
    );
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut llr = [1.0; 174];
        llr[37] = value;
        let expected = osd::DecodeError::InvalidLlr(fec::InvalidLlr { index: 37 });
        assert_eq!(decode_ap(&llr, &mask, 2), Err(expected.clone()));
        assert_eq!(decode_hybrid_ap(&llr, &mask, 3, 2), Err(expected));
    }
    assert_eq!(
        decode_ap(&[f32::MAX; 174], &mask, 2),
        Err(osd::DecodeError::LikelihoodOverflow)
    );
    assert_eq!(
        decode_hybrid_ap(&[f32::MAX; 174], &mask, 3, 2),
        Err(osd::DecodeError::LikelihoodOverflow)
    );
}

#[test]
fn masked_bp_only_agrees_with_native_hybrid_bp_results() {
    let vectors = native_vectors();
    for case in vectors["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["max_osd"] == 0)
    {
        let input = vectors["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == case["id"])
            .unwrap();
        let llr = std::array::from_fn(|i| input["llr"][i].as_f64().unwrap() as f32);
        let mask = std::array::from_fn(|i| input["mask"][i].as_bool().unwrap());
        let result = fec::decode_ap(&llr, &mask, 30).unwrap();
        assert_eq!(
            result.is_some(),
            case["method"] == 1 && llr.iter().any(|&v| v != 0.0),
            "{}",
            case["id"]
        );
        if let Some(decoded) = result {
            assert_eq!(word(&decoded.codeword), case["codeword"].as_str().unwrap());
        }
        assert_eq!(
            fec::decode_ap(&llr, &[false; 174], 30),
            fec::decode(&llr, 30)
        );
    }
}
