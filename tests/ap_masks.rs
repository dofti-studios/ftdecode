// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{
    assistance::{self, Activity, ApMode, ContactState},
    engine::{DecodeSettings, Mode},
};
#[test]
fn frozen_likelihoods_match_native_activity_and_callsign_vectors() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/ap-mask-vectors.json")).unwrap();
    for v in fixture["vectors"].as_array().unwrap() {
        let mode = if v["mode"] == 4 { Mode::Ft4 } else { Mode::Ft8 };
        let activity = [
            Activity::Normal,
            Activity::NaVhf,
            Activity::EuVhf,
            Activity::FieldDay,
            Activity::Rtty,
            Activity::WwDigi,
            Activity::Fox,
            Activity::Hound,
            Activity::ArrlDigi,
        ][v["activity"].as_u64().unwrap() as usize];
        let ap_type = v["ap_type"].as_u64().unwrap() as u8;
        let mut s = DecodeSettings {
            assistance: true,
            priority_hz: Some(900.0),
            ..DecodeSettings::default()
        };
        s.ap.mode = ApMode::Auto;
        s.ap.activity = activity;
        s.ap.my_call = v["my_call"]
            .as_str()
            .filter(|c| *c != "-")
            .map(str::to_owned);
        s.ap.dx_call = v["dx_call"]
            .as_str()
            .filter(|c| *c != "-")
            .map(str::to_owned);
        s.ap.state = match ap_type {
            1 | 2 => ContactState::CallingCq,
            _ => ContactState::RogerReport,
        };
        let hypotheses = assistance::hypotheses(&s, mode, 900.0);
        let h = hypotheses.iter().find(|h| h.ap_type == ap_type);
        let native = v["mask"].as_str().unwrap();
        let label = format!(
            "{:?} {:?} type={ap_type} {:?}/{:?}",
            mode, activity, s.ap.my_call, s.ap.dx_call
        );
        if !native.contains('1') {
            assert!(h.is_none(), "{label}");
            continue;
        }
        let h = h.unwrap_or_else(|| panic!("missing {label}"));
        assert_eq!(
            h.mask
                .iter()
                .map(|b| if *b { '1' } else { '0' })
                .collect::<String>(),
            native,
            "{label}"
        );
        let llr = std::array::from_fn(|i| {
            if i % 2 == 0 {
                -1.0 - i as f32 / 256.0 - 1.0 / 256.0
            } else {
                1.0 + i as f32 / 256.0 + 1.0 / 256.0
            }
        });
        let applied = h.apply(&llr, 10.0);
        for (i, expected) in v["llr"].as_array().unwrap().iter().enumerate() {
            assert!(
                (applied[i] - expected.as_f64().unwrap() as f32).abs() < 1e-5,
                "{label} bit {i}: {} vs {expected}",
                applied[i]
            );
        }
    }
}
#[test]
fn contact_ap_requires_explicit_target_and_ft4_uses_rx_only() {
    let mut s = DecodeSettings {
        assistance: true,
        ..Default::default()
    };
    s.ap.mode = ApMode::Auto;
    s.ap.my_call = Some("W9XYZ".into());
    s.ap.dx_call = Some("K1ABC".into());
    s.ap.state = ContactState::RogerReport;
    for mode in [Mode::Ft8, Mode::Ft4] {
        assert!(assistance::hypotheses(&s, mode, 1500.0).is_empty());
    }
    s.ap.tx_hz = Some(1500.0);
    assert_eq!(assistance::hypotheses(&s, Mode::Ft8, 1550.0).len(), 4);
    assert!(assistance::hypotheses(&s, Mode::Ft4, 1500.0).is_empty());
    s.priority_hz = Some(1500.0);
    assert_eq!(assistance::hypotheses(&s, Mode::Ft4, 1550.0).len(), 2);
    assert!(assistance::hypotheses(&s, Mode::Ft4, 1550.1).is_empty());
    s.ap.mode = ApMode::Off;
    assert!(assistance::hypotheses(&s, Mode::Ft8, 1500.0).is_empty());
    s.ap.activity = Activity::Hound;
    assert!(assistance::hypotheses(&s, Mode::Ft8, 900.0).is_empty());
    s.ap.mode = ApMode::Auto;
    s.assistance = false;
    assert!(assistance::hypotheses(&s, Mode::Ft4, 1500.0).is_empty());
}
#[test]
fn call_hints_accept_native_hash_width_and_reject_malformed_delimiters() {
    let mut s = DecodeSettings::default();
    s.ap.known_calls.push("PJ4/K1ABC1234".into());
    assert!(s.validate().is_ok());
    for call in [
        "<K1ABC",
        "K1ABC>",
        "<<K1ABC>>",
        "/K1ABC",
        "K1ABC/",
        "K1/AB/C",
    ] {
        let mut s = DecodeSettings::default();
        s.ap.my_call = Some(call.into());
        assert!(s.validate().is_err(), "{call}");
    }
}
