use std::process::{Command, Output};
fn run(extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ftdecode"))
        .args(["decode", "tests/fixtures/ft4-clean.wav", "--mode", "ft4"])
        .args(extra)
        .output()
        .unwrap()
}
#[test]
fn default_output_is_readable_and_fields_can_select_only_messages() {
    let out = run(&[]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("Message") && text.contains("CQ K1ABC FN42"),
        "{text}"
    );
    assert!(!text.contains("\"type\""));
    let out = run(&["--fields", "message", "--quiet"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "CQ K1ABC FN42\n");
    assert!(out.stderr.is_empty());
}
#[test]
fn structured_verbose_output_has_real_message_diagnostics_and_window_statistics() {
    let out = run(&["--output", "jsonl", "-vv"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let d = events.iter().find(|v| v["type"] == "decode").unwrap();
    assert_eq!(d["message_type"], "standard");
    assert!(
        matches!(d["diagnostics"]["fec"].as_str(), Some("BP" | "OSD")),
        "{d}"
    );
    assert!(d["diagnostics"]["bp_iterations"].is_u64());
    assert_eq!(d["payload"].as_str().unwrap().len(), 77);
    let done = events.iter().find(|v| v["type"] == "done").unwrap();
    assert_eq!(done["message_count"], 1);
    assert!(done["stats"]["candidates"].as_u64().unwrap() > 0, "{done}");
    assert!(done["stats"]["passes"].as_u64().unwrap() > 0);
    assert!(done["stats"]["decode_ms"].as_f64().unwrap() >= 0.0);
    assert_eq!(
        done["stats"]["bp_decodes"].as_u64().unwrap()
            + done["stats"]["osd_decodes"].as_u64().unwrap(),
        1
    );
}
#[test]
fn invalid_output_options_fail_before_opening_audio() {
    for args in [
        vec!["--output", "csv"],
        vec!["--fields", "message,nope"],
        vec!["--fields", ""],
        vec!["--fields", "message,message"],
        vec!["--output", "jsonl", "--fields", "message"],
    ] {
        let out = run(&args);
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn actual_ft8_fixtures_expose_osd_fallback_and_subtraction_passes() {
    let decode = |path: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_ftdecode"))
            .args(["decode", path, "--output", "jsonl", "-vv"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|v| v["type"] == "decode")
            .collect::<Vec<_>>()
    };
    let weak = decode("tests/fixtures/ft8-weak.wav");
    let diagnostic = &weak
        .iter()
        .find(|v| v["message"] == "CQ K1ABC FN42")
        .unwrap()["diagnostics"];
    assert_eq!(diagnostic["fec"], "OSD");
    assert!(diagnostic["osd_pass"].as_u64().unwrap() > 0);
    assert!(diagnostic["hard_errors"].as_u64().unwrap() > 0);
    assert!(diagnostic["bp_iterations"].as_u64().unwrap() <= 30);
    let mixture = decode("tests/fixtures/ft8-mixture.wav");
    let strong = mixture
        .iter()
        .find(|v| v["message"] == "K1ABC W9XYZ EN37")
        .unwrap();
    let weak = mixture
        .iter()
        .find(|v| v["message"] == "CQ K1ABC FN42")
        .unwrap();
    assert_eq!(strong["diagnostics"]["fec"], "BP");
    assert!(strong["diagnostics"]["osd_pass"].is_null());
    assert_eq!(strong["diagnostics"]["pass"], 1);
    assert!(weak["diagnostics"]["pass"].as_u64().unwrap() > 1);
}

#[test]
fn skipped_tail_keeps_known_utc_and_requested_zero_statistics() {
    let mut wav = std::fs::read("tests/fixtures/ft4-clean.wav").unwrap();
    wav.truncate(44 + 2000);
    wav[4..8].copy_from_slice(&2036u32.to_le_bytes());
    wav[40..44].copy_from_slice(&2000u32.to_le_bytes());
    let path = std::env::temp_dir().join(format!("ftdecode-utc-tail-{}.wav", std::process::id()));
    std::fs::write(&path, wav).unwrap();
    for json in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ftdecode"));
        command.args([
            "decode",
            path.to_str().unwrap(),
            "--mode",
            "ft4",
            "--start-utc-ns",
            "43200000000000",
            "--stats",
        ]);
        if json {
            command.args(["--output", "jsonl"]);
        }
        let out = command.output().unwrap();
        assert!(out.status.success());
        if json {
            let events: Vec<serde_json::Value> = String::from_utf8(out.stdout)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            let done = events.iter().find(|v| v["type"] == "done").unwrap();
            assert_eq!(done["status"], "skipped");
            assert_eq!(done["window_start_utc_ns"], "43200000000000");
            assert_eq!(done["stats"]["candidates"], 0);
            assert_eq!(done["stats"]["decode_ms"], 0.0);
        } else {
            let text = String::from_utf8(out.stderr).unwrap();
            assert!(text.contains("12:00:00.000"), "{text}");
            assert!(!text.contains("+00:00:00.000"), "{text}");
        }
    }
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn cli_slot_separators_default_on_and_can_be_disabled() {
    // Two silent FT4 slots exercise boundaries even without decoded messages.
    let mut wav = std::fs::read("tests/fixtures/ft4-clean.wav").unwrap();
    let data_len = 2 * 90_000 * 2u32;
    wav.truncate(44);
    wav.resize(44 + data_len as usize, 0);
    wav[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    wav[40..44].copy_from_slice(&data_len.to_le_bytes());
    let path = std::env::temp_dir().join(format!("ftdecode-separators-{}.wav", std::process::id()));
    std::fs::write(&path, wav).unwrap();
    for disabled in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ftdecode"));
        command.args([
            "decode",
            path.to_str().unwrap(),
            "--mode",
            "ft4",
            "--slot-offset-samples",
            "0",
            "--quiet",
        ]);
        if disabled {
            command.arg("--no-slot-separators");
        }
        let out = command.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            if disabled {
                ""
            } else {
                "--- +00:00:07.500 ---\n"
            }
        );
        assert!(out.stderr.is_empty());
    }
    std::fs::remove_file(path).unwrap();
}
