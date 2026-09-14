use ftdecode::{
    assistance::{ApMode, ContactState},
    engine::DecodeSettings,
};

#[test]
fn rx_is_optional_and_does_not_change_search_band() {
    let mut settings = DecodeSettings {
        priority_hz: Some(1500.0),
        ..DecodeSettings::default()
    };
    settings.ap.my_call = Some("K1ABC".into());
    settings.ap.dx_call = Some("W9XYZ".into());
    settings.ap.state = ContactState::Report;
    settings.validate().unwrap();
    assert_eq!((settings.low_hz, settings.high_hz), (200.0, 3000.0));
    assert!(settings.ap.targeted(1500.0, settings.priority_hz));
    assert!(!settings.ap.targeted(1700.0, settings.priority_hz));
    assert!(!settings.ap.targeted(1500.0, None));
}

#[test]
fn context_validation_rejects_bad_hints_and_width() {
    for call in ["", "has spaces", "K1ABC!", "abcdefghijklmn"] {
        let mut settings = DecodeSettings::default();
        settings.ap.my_call = Some(call.into());
        assert!(settings.validate().is_err(), "{call}");
    }
    let mut settings = DecodeSettings::default();
    settings.ap.dx_grid = Some("ZZ99".into());
    assert!(settings.validate().is_err());
    settings.ap.dx_grid = None;
    settings.ap.width_hz = f32::NAN;
    assert!(settings.validate().is_err());
}

#[test]
fn ap_modes_and_contact_states_are_explicit() {
    assert_eq!(ApMode::default(), ApMode::Off);
    assert_eq!(DecodeSettings::default().ap.mode, ApMode::Off);
    assert!(!DecodeSettings::default().assistance);
    assert_eq!(DecodeSettings::default().ap_mode(), ApMode::Off);
    assert_eq!("auto".parse::<ApMode>().unwrap(), ApMode::Auto);
    assert_eq!("off".parse::<ApMode>().unwrap(), ApMode::Off);
    assert!("yes".parse::<ApMode>().is_err());
    assert_eq!(
        "roger-report".parse::<ContactState>().unwrap(),
        ContactState::RogerReport
    );
    assert!("unknown".parse::<ContactState>().is_err());
    let settings = DecodeSettings {
        assistance: false,
        ..DecodeSettings::default()
    };
    assert_eq!(settings.ap_mode(), ApMode::Off);
}

#[test]
fn brackets_on_supplied_calls_do_not_disable_contact_hypotheses() {
    use ftdecode::{assistance::hypotheses, engine::Mode};
    for mode in [Mode::Ft8, Mode::Ft4] {
        let mut settings = DecodeSettings {
            assistance: true,
            priority_hz: Some(1500.0),
            ..DecodeSettings::default()
        };
        settings.ap.mode = ApMode::Auto;
        settings.ap.my_call = Some("KA1ABC".into());
        settings.ap.dx_call = Some("W9XYZ".into());
        settings.ap.state = ContactState::Report;
        let expected = hypotheses(&settings, mode, 1500.0);
        assert!(!expected.is_empty());
        settings.ap.my_call = Some("<KA1ABC>".into());
        settings.ap.dx_call = Some("<W9XYZ>".into());
        settings.validate().unwrap();
        let actual = hypotheses(&settings, mode, 1500.0);
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(&expected) {
            assert_eq!(actual.ap_type, expected.ap_type);
            assert_eq!(actual.bits, expected.bits);
            assert_eq!(actual.mask, expected.mask);
        }
    }
}

#[test]
fn cli_accepts_context_and_ap_off_preserves_unassisted_output() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ftdecode"))
        .args([
            "decode",
            "tests/fixtures/ft8-clean.wav",
            "--ap",
            "off",
            "--rx-freq",
            "1500",
            "--my-call",
            "K1ABC",
            "--dx-call",
            "W9XYZ",
            "--qso-state",
            "report",
            "--output",
            "jsonl",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        events
            .iter()
            .any(|e| e["message"] == "CQ K1ABC FN42" && e["assisted"] == false)
    );
}

#[test]
fn maximum_known_calls_fit_the_wav_start_frame() {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ftdecode"));
    command.args([
        "decode",
        "tests/fixtures/ft8-clean.wav",
        "--ap",
        "off",
        "--quiet",
    ]);
    // Longest accepted hash spelling, with outer whitespace normalized away.
    for _ in 0..400 {
        command.args(["--known-call", " <K1ABCDEFGHIJK> "]);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    command.args(["--known-call", "K1ABC"]);
    let output = command.output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("400"));
}

#[test]
fn cli_requires_explicit_ap_opt_in_even_with_contact_hints() {
    for (ap, expected) in [
        (None, false),
        (Some("off"), false),
        (Some("cq"), false),
        (Some("auto"), true),
    ] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ftdecode"));
        command.args([
            "decode",
            "tests/fixtures/ft4-ap-weak.wav",
            "--mode",
            "ft4",
            "--min-hz",
            "1475",
            "--max-hz",
            "1525",
            "--rx-freq",
            "1500",
            "--my-call",
            "W9XYZ",
            "--dx-call",
            "K1ABC",
            "--qso-state",
            "roger-report",
            "--output",
            "jsonl",
        ]);
        if let Some(ap) = ap {
            command.args(["--ap", ap]);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let decoded: Vec<_> = events.iter().filter(|e| e["type"] == "decode").collect();
        assert_eq!(
            decoded.len(),
            usize::from(expected),
            "ap={ap:?}: {decoded:?}"
        );
        if expected {
            assert_eq!(decoded[0]["message"], "W9XYZ K1ABC RR73");
            assert_eq!(decoded[0]["assisted"], true);
        }
    }
}
