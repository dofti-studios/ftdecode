// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{fec, osd};

fn native_cases() -> serde_json::Value {
    let mut vectors: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/osd-vectors.json")).unwrap();
    let inputs = vectors["inputs"].as_array().unwrap().clone();
    for case in vectors["cases"].as_array_mut().unwrap() {
        case["llr"] = inputs
            .iter()
            .find(|input| input["id"] == case["id"])
            .unwrap()["llr"]
            .clone();
    }
    vectors
}

fn word<const N: usize>(bits: &[bool; N]) -> String {
    bits.iter().map(|&b| if b { '1' } else { '0' }).collect()
}

#[test]
fn osd_matches_native_depths_and_ties() {
    let vectors = native_cases();
    for c in vectors["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["max_osd"] == -2)
    {
        let llr = std::array::from_fn(|i| c["llr"][i].as_f64().unwrap() as f32);
        let actual =
            osd::decode(&llr, c["depth"].as_u64().unwrap() as u8).unwrap_or_else(|error| {
                panic!(
                    "{} max {} depth {}: {error}",
                    c["id"], c["max_osd"], c["depth"]
                )
            });
        let label = format!("{} depth {}", c["id"], c["depth"]);
        // The public API explicitly rejects absent evidence, unlike upstream BP.
        let accepted = c["method"] != 0 && c["id"] != "all-zero";
        assert_eq!(actual.is_some(), accepted, "{label}");
        if let Some(actual) = actual {
            assert_eq!(
                word(&actual.message),
                &c["codeword"].as_str().unwrap()[..77],
                "{label}"
            );
            assert_eq!(
                word(&actual.codeword),
                c["codeword"].as_str().unwrap(),
                "{label}"
            );
            assert_eq!(
                actual.hard_errors as i64,
                c["hard_errors"].as_i64().unwrap(),
                "{label}"
            );
            let expected = c["distance"].as_f64().unwrap() as f32;
            assert!(
                (actual.distance - expected).abs() <= 0.00002 * expected.max(1.0),
                "{label}: {} != {expected}",
                actual.distance
            );
        }
    }
}

#[test]
fn hybrid_matches_native_including_bp_failures_recovered_by_osd() {
    let vectors = native_cases();
    let mut recovered = std::collections::HashSet::new();
    let mut passes_seen = [false; 3];
    for c in vectors["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["max_osd"].as_i64().unwrap() >= 0)
    {
        let llr = std::array::from_fn(|i| c["llr"][i].as_f64().unwrap() as f32);
        let actual = fec::decode_hybrid(
            &llr,
            c["max_osd"].as_u64().unwrap() as u8,
            c["depth"].as_u64().unwrap() as u8,
        )
        .unwrap_or_else(|error| {
            panic!(
                "{} max {} depth {}: {error}",
                c["id"], c["max_osd"], c["depth"]
            )
        });
        let label = format!("{} max {} depth {}", c["id"], c["max_osd"], c["depth"]);
        assert_eq!(
            actual.is_some(),
            c["method"] != 0 && c["id"] != "all-zero",
            "{label}"
        );
        if let Some(actual) = actual {
            assert_eq!(
                word(&actual.decoded.message),
                &c["codeword"].as_str().unwrap()[..77],
                "{label}"
            );
            assert_eq!(
                word(&actual.decoded.codeword),
                c["codeword"].as_str().unwrap(),
                "{label}"
            );
            assert_eq!(
                actual.decoded.hard_errors as i64,
                c["hard_errors"].as_i64().unwrap(),
                "{label}"
            );
            let expected = c["distance"].as_f64().unwrap() as f32;
            assert!(
                (actual.distance - expected).abs() <= 0.00002 * expected.max(1.0),
                "{label}: {} != {expected}",
                actual.distance
            );
            assert!(actual.decoded.iterations <= 30);
            if c["method"] == 2 {
                assert_eq!(actual.method, fec::DecodeMethod::Osd);
                assert!(fec::decode(&llr, 30).unwrap().is_none(), "{label}");
                let expected_pass = if c["max_osd"] == 0 {
                    1
                } else {
                    (1..=c["max_osd"].as_u64().unwrap())
                        .find(|&pass| {
                            vectors["cases"].as_array().unwrap().iter().any(|other| {
                                other["id"] == c["id"]
                                    && other["depth"] == c["depth"]
                                    && other["max_osd"] == pass
                                    && other["method"] == 2
                            })
                        })
                        .unwrap() as u8
                };
                assert_eq!(actual.osd_pass, Some(expected_pass), "{label}");
                passes_seen[usize::from(expected_pass - 1)] = true;
                recovered.insert(c["id"].as_str().unwrap());
            } else {
                assert_eq!(actual.method, fec::DecodeMethod::Bp);
                assert_eq!(actual.osd_pass, None);
            }
        }
    }
    assert!(
        recovered.len() >= 22,
        "reference corpus must exercise real fallback"
    );
    assert_eq!(passes_seen, [true; 3], "exercise every saved OSD attempt");
}

#[test]
fn settings_and_nonfinite_inputs_are_rejected() {
    let llr = [-1.0; 174];
    for depth in 3..=255 {
        assert_eq!(
            osd::decode(&llr, depth),
            Err(osd::DecodeError::UnsupportedDepth(depth))
        );
        assert_eq!(
            fec::decode_hybrid(&llr, 0, depth),
            Err(osd::DecodeError::UnsupportedDepth(depth))
        );
    }
    for passes in 4..=255 {
        assert_eq!(
            fec::decode_hybrid(&llr, passes, 2),
            Err(osd::DecodeError::UnsupportedMaxOsd(passes))
        );
    }
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut llr = llr;
        llr[123] = value;
        assert_eq!(
            osd::decode(&llr, 2),
            Err(osd::DecodeError::InvalidLlr(fec::InvalidLlr { index: 123 }))
        );
        assert_eq!(
            fec::decode_hybrid(&llr, 3, 2),
            Err(osd::DecodeError::InvalidLlr(fec::InvalidLlr { index: 123 }))
        );
    }
    assert!(osd::decode(&[f32::MAX; 174], 2).is_err());
    assert!(fec::decode_hybrid(&[f32::MAX; 174], 3, 2).is_err());
}

#[test]
fn zero_input_and_bad_crc_are_not_payloads() {
    for depth in 0..=2 {
        assert!(osd::decode(&[0.0; 174], depth).unwrap().is_none());
        for passes in 0..=3 {
            assert!(
                fec::decode_hybrid(&[0.0; 174], passes, depth)
                    .unwrap()
                    .is_none()
            );
        }
    }
    let vectors = native_cases();
    let c = vectors["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "valid-parity-bad-crc")
        .unwrap();
    let llr = std::array::from_fn(|i| c["llr"][i].as_f64().unwrap() as f32);
    for depth in 0..=2 {
        assert!(osd::decode(&llr, depth).unwrap().is_none());
        for passes in 0..=3 {
            assert!(fec::decode_hybrid(&llr, passes, depth).unwrap().is_none());
        }
    }
}

#[test]
fn bp_controls_match_native_for_the_expanded_corpus() {
    let vectors = native_cases();
    for c in vectors["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["max_osd"] == -1)
    {
        let llr = std::array::from_fn(|i| c["llr"][i].as_f64().unwrap() as f32);
        let actual = fec::decode(&llr, 30).unwrap();
        assert_eq!(
            actual.is_some(),
            c["method"] != 0 && c["id"] != "all-zero",
            "{}",
            c["id"]
        );
        if let Some(actual) = actual {
            assert_eq!(
                word(&actual.codeword),
                c["codeword"].as_str().unwrap(),
                "{}",
                c["id"]
            );
            assert_eq!(
                actual.hard_errors as i64,
                c["hard_errors"].as_i64().unwrap(),
                "{}",
                c["id"]
            );
        }
    }
}

#[test]
fn native_corpus_contains_weak_ft4_recovery() {
    let vectors = native_cases();
    let cases = vectors["cases"].as_array().unwrap();
    let recovered = cases
        .iter()
        .filter(|case| {
            case["id"].as_str().unwrap().starts_with("ft4-weak-")
                && case["method"] == 2
                && case["max_osd"].as_i64().unwrap() >= 0
                && cases.iter().any(|control| {
                    control["id"] == case["id"]
                        && control["max_osd"] == -1
                        && control["method"] == 0
                })
        })
        .count();
    assert!(
        recovered > 0,
        "FT4 corpus must include actual BP failures recovered by OSD"
    );
}
