// SPDX-License-Identifier: GPL-3.0-or-later
use std::process::Command;
#[test]
fn help_describes_audio_and_network_entry_points() {
    let result = Command::new(env!("CARGO_BIN_EXE_ftdecode"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(result.status.success());
    let text = String::from_utf8(result.stdout).unwrap();
    for word in ["decode", "serve", "stdio", "12000"] {
        assert!(text.contains(word));
    }
}
#[test]
fn cli_rejects_unknown_flags_and_external_network_bindings() {
    for args in [
        vec!["decode", "none.wav", "--nonsense"],
        vec!["serve", "--bind", "0.0.0.0:8000"],
        vec!["stdio", "--depth", "7"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_ftdecode"))
            .args(args)
            .output()
            .unwrap();
        assert!(!result.status.success());
    }
}

#[test]
fn wav_reports_stream_timing_overflow_as_failure() {
    let mut wav = include_bytes!("fixtures/ft8-clean.wav").to_vec();
    wav.extend_from_slice(&include_bytes!("fixtures/ft8-clean.wav")[44..]);
    let size = wav.len() as u32;
    wav[4..8].copy_from_slice(&(size - 8).to_le_bytes());
    wav[40..44].copy_from_slice(&(size - 44).to_le_bytes());
    let path = std::env::temp_dir().join(format!("ftdecode-overflow-{}.wav", std::process::id()));
    std::fs::write(&path, wav).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ftdecode"))
        .args([
            "decode",
            path.to_str().unwrap(),
            "--output",
            "jsonl",
            "--start-utc-ns",
            "18446744073709551615",
        ])
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("\"type\":\"error\""));
}
#[test]
fn malformed_stdio_framing_exits_unsuccessfully() {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_ftdecode"))
        .arg("stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&8193_u32.to_le_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let frame = ftdecode::protocol::read_frame(&mut &output.stdout[..])
        .unwrap()
        .unwrap();
    assert_eq!(frame.header["type"], "error");
}
