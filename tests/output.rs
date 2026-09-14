// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{
    output::{EventOutput, OutputFormat, OutputOptions},
    protocol,
};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<(Vec<u8>, usize)>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().0.clone()).unwrap()
    }

    fn flushes(&self) -> usize {
        self.0.lock().unwrap().1
    }
}

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().1 += 1;
        Ok(())
    }
}

fn decode_event() -> Value {
    json!({
        "type": "decode",
        "payload_bytes": 0,
        "message": "CQ K1ABC FN42",
        "message_type": "standard",
        "grid": "FN42",
        "mode": "ft8",
        "frequency_hz": 1499.66796875,
        "snr_db": -12,
        "dt_seconds": -0.000500023365020752,
        "window_start_sample": "180000",
        "assisted": false,
        "hashes": [{"width": 22, "value": 123, "resolved": null}],
        "payload": "1010101",
        "diagnostics": {
            "fec": "OSD",
            "bp_iterations": 40,
            "osd_pass": 2,
            "hard_errors": 3,
            "pass": 1
        }
    })
}

#[test]
fn options_validate_fields_and_required_detail() {
    let fields = OutputOptions::parse_fields("message,fec,payload").unwrap();
    assert_eq!(fields, ["message", "fec", "payload"]);

    let options = OutputOptions {
        fields: Some(fields),
        ..OutputOptions::default()
    };
    assert_eq!(options.diagnostics_level(), 2);
    assert!(!options.window_stats_enabled());

    for value in ["", "message,,snr", "message,message", "unknown"] {
        let error = OutputOptions::parse_fields(value).unwrap_err();
        if value == "unknown" {
            assert!(
                error.contains("supported fields: time, mode, snr"),
                "{error}"
            );
        }
    }

    let options = OutputOptions {
        format: OutputFormat::Jsonl,
        fields: Some(vec!["message".into()]),
        ..OutputOptions::default()
    };
    assert!(options.validate().is_err());
}

#[test]
fn default_options_select_readable_text_columns() {
    let options = OutputOptions::default();
    assert_eq!(options.format, OutputFormat::Text);
    assert_eq!(
        options.text_fields(),
        ["time", "mode", "snr", "dt", "freq", "ap_type", "message"]
    );
    assert_eq!(options.diagnostics_level(), 0);
    assert!(!options.window_stats_enabled());
    assert_eq!("text".parse::<OutputFormat>().unwrap(), OutputFormat::Text);
    assert_eq!(
        "jsonl".parse::<OutputFormat>().unwrap(),
        OutputFormat::Jsonl
    );
    assert!("csv".parse::<OutputFormat>().is_err());
}

#[test]
fn explicit_fields_request_only_the_detail_the_stream_needs() {
    for (field, level) in [
        ("hashes", 0),
        ("assisted", 0),
        ("grid", 1),
        ("hard_errors", 1),
        ("payload", 2),
    ] {
        let options = OutputOptions {
            fields: Some(vec![field.into()]),
            ..OutputOptions::default()
        };
        assert_eq!(options.diagnostics_level(), level, "{field}");
    }
}

#[test]
fn default_text_has_readable_columns_and_flushes_each_decode() {
    let stdout = Capture::default();
    let stderr = Capture::default();
    let mut output = EventOutput::new(stdout.clone(), stderr.clone(), OutputOptions::default());

    output.event(&decode_event()).unwrap();

    assert_eq!(
        stdout.text(),
        "Time           Mode  SNR  DT      Freq    AP   Message\n+00:00:15.000  FT8   -12  +0.00   1499.7       CQ K1ABC FN42\n"
    );
    assert_eq!(stdout.flushes(), 1);
    assert!(stderr.text().is_empty());
}

#[test]
fn custom_fields_preserve_order_and_message_only_has_no_header() {
    let stdout = Capture::default();
    let mut output = EventOutput::new(
        stdout.clone(),
        Capture::default(),
        OutputOptions {
            fields: Some(OutputOptions::parse_fields("freq,message,snr").unwrap()),
            ..OutputOptions::default()
        },
    );
    output.event(&decode_event()).unwrap();
    assert_eq!(
        stdout.text(),
        "Freq    Message                                SNR\n1499.7  CQ K1ABC FN42                          -12\n"
    );

    let stdout = Capture::default();
    let mut output = EventOutput::new(
        stdout.clone(),
        Capture::default(),
        OutputOptions {
            fields: Some(vec!["message".into()]),
            ..OutputOptions::default()
        },
    );
    output.event(&decode_event()).unwrap();
    assert_eq!(stdout.text(), "CQ K1ABC FN42\n");
}

#[test]
fn verbose_fields_show_diagnostics_and_missing_values_use_em_dash() {
    let stdout = Capture::default();
    let mut output = EventOutput::new(
        stdout.clone(),
        Capture::default(),
        OutputOptions {
            fields: Some(
                OutputOptions::parse_fields(
                    "type,fec,bp_iterations,osd_pass,hard_errors,pass,grid,payload,assisted,hashes",
                )
                .unwrap(),
            ),
            ..OutputOptions::default()
        },
    );
    output.event(&decode_event()).unwrap();
    let lines: Vec<_> = stdout.text().lines().map(str::to_owned).collect();
    assert_eq!(
        lines[1],
        "standard  OSD  40             2         3            1     FN42  1010101  false     22:123:—"
    );

    let mut missing = decode_event();
    missing.as_object_mut().unwrap().remove("diagnostics");
    missing.as_object_mut().unwrap().remove("grid");
    missing.as_object_mut().unwrap().remove("payload");
    output.event(&missing).unwrap();
    let text = stdout.text();
    assert!(text.ends_with("standard  —    —              —         —            —     —     —        false     22:123:—\n"));
}

#[test]
fn utc_time_uses_integer_milliseconds() {
    let stdout = Capture::default();
    let mut output = EventOutput::new(
        stdout.clone(),
        Capture::default(),
        OutputOptions {
            fields: Some(vec!["time".into(), "message".into()]),
            ..OutputOptions::default()
        },
    );
    let mut event = decode_event();
    event["window_start_utc_ns"] = json!(86_399_999_999_999_u64.to_string());
    output.event(&event).unwrap();
    assert_eq!(
        stdout.text(),
        "Time           Message\n23:59:59.999   CQ K1ABC FN42\n"
    );
}

#[test]
fn jsonl_retains_events_and_quiet_does_not_suppress_failures() {
    let stdout = Capture::default();
    let stderr = Capture::default();
    let mut output = EventOutput::new(
        stdout.clone(),
        stderr.clone(),
        OutputOptions {
            format: OutputFormat::Jsonl,
            quiet: true,
            ..OutputOptions::default()
        },
    );
    let error = json!({"type":"error","payload_bytes":0,"message":"bad audio"});
    let done = json!({"type":"done","payload_bytes":0,"status":"failed","reason":"decode failed"});
    output.event(&error).unwrap();
    output.event(&done).unwrap();

    let events: Vec<Value> = stdout
        .text()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events, [error, done]);
    assert!(
        output
            .error_flag()
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    assert!(stderr.text().is_empty());
}

#[test]
fn text_routes_metadata_stats_errors_and_final_summary_to_diagnostics() {
    let stdout = Capture::default();
    let stderr = Capture::default();
    let mut output = EventOutput::new(
        stdout.clone(),
        stderr.clone(),
        OutputOptions {
            stats: true,
            ..OutputOptions::default()
        },
    );
    output
        .event(
            &json!({"type":"input","source_rate":48000,"channels":2,"bits_per_sample":24,
            "sample_format":"pcm","channel":"mix","output_rate":12000,"source_frames":480000,
            "output_frames":"120000"}),
        )
        .unwrap();
    output
        .event(
            &json!({"type":"done","status":"completed","window_start_sample":"0",
            "message_count":1,"stats":{"candidates":9,"passes":2,"internal_decodes":2,
            "bp_decodes":1,"osd_decodes":0,"decode_ms":12.5}}),
        )
        .unwrap();
    output
        .event(&json!({"type":"warning","reason":"incomplete tail"}))
        .unwrap();
    output
        .event(&json!({"type":"error","message":"decode broke"}))
        .unwrap();
    output
        .event(&json!({"type":"ok","command":"finish"}))
        .unwrap();

    assert!(stdout.text().is_empty());
    let diagnostics = stderr.text();
    assert!(diagnostics.contains("Input: 48000 Hz, 2 channels, 24-bit pcm, mix; 480000 source frames → 120000 at 12000 Hz"), "{diagnostics}");
    assert!(diagnostics.contains("Window +00:00:00.000: 9 candidates, 2 passes, 2 internal, 1 emitted (1 BP, 0 OSD), 12.50 ms"), "{diagnostics}");
    assert!(
        diagnostics.contains("Warning: incomplete tail"),
        "{diagnostics}"
    );
    assert!(diagnostics.contains("Error: decode broke"), "{diagnostics}");
    assert!(diagnostics.contains("Summary: 1 window (1 completed, 0 skipped, 0 cancelled, 0 failed), 1 message, 9 candidates, 10.000 s converted, 12.50 ms decoder"), "{diagnostics}");
}

#[test]
fn summary_counts_terminal_statuses_and_default_text_reports_incomplete_tail() {
    let stderr = Capture::default();
    let mut output = EventOutput::new(Capture::default(), stderr.clone(), OutputOptions::default());
    output
        .event(&json!({"type":"input","output_frames":"30000"}))
        .unwrap();
    for (status, reason) in [
        ("completed", Value::Null),
        ("skipped", json!("incomplete tail")),
        ("cancelled", json!("reset")),
        ("failed", json!("decoder error")),
    ] {
        output
            .event(&json!({"type":"done","status":status,"reason":reason,
                "window_start_sample":"180000","message_count":0}))
            .unwrap();
    }
    output
        .event(&json!({"type":"ok","command":"finish"}))
        .unwrap();
    let diagnostics = stderr.text();
    assert!(
        diagnostics.contains("Skipped window +00:00:15.000: incomplete tail"),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains("Cancelled window +00:00:15.000: reset"),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains("Error: decoder error"),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains("4 windows (1 completed, 1 skipped, 1 cancelled, 1 failed)"),
        "{diagnostics}"
    );
    assert!(diagnostics.contains("2.500 s converted"), "{diagnostics}");
    assert!(
        !diagnostics.lines().last().unwrap().contains("candidates"),
        "stats were not requested: {diagnostics}"
    );
}

#[test]
fn quiet_suppresses_routine_diagnostics_but_preserves_warnings_and_errors() {
    let stderr = Capture::default();
    let mut output = EventOutput::new(
        Capture::default(),
        stderr.clone(),
        OutputOptions {
            stats: true,
            quiet: true,
            ..OutputOptions::default()
        },
    );
    output
        .event(&json!({"type":"input","source_rate":12000}))
        .unwrap();
    output
        .event(&json!({"type":"done","status":"completed","stats":{"candidates":1}}))
        .unwrap();
    output
        .event(&json!({"type":"ok","command":"finish"}))
        .unwrap();
    output
        .event(&json!({"type":"warning","reason":"clipped input"}))
        .unwrap();
    output
        .event(&json!({"type":"error","message":"failed"}))
        .unwrap();
    assert_eq!(stderr.text(), "Warning: clipped input\nError: failed\n");
}

#[test]
fn framed_write_accepts_fragmentation_and_flushes_rendered_output() {
    let stdout = Capture::default();
    let mut output = EventOutput::new(stdout.clone(), Capture::default(), OutputOptions::default());
    let mut frame = Vec::new();
    protocol::write_frame(&mut frame, &decode_event()).unwrap();
    for byte in frame {
        output.write_all(&[byte]).unwrap();
    }
    assert!(stdout.text().contains("CQ K1ABC FN42"));
    assert_eq!(stdout.flushes(), 1);
}

#[test]
fn framed_write_rejects_unbounded_or_malformed_headers() {
    let mut output = EventOutput::new(
        Capture::default(),
        Capture::default(),
        OutputOptions::default(),
    );
    let error = output.write_all(&8193_u32.to_le_bytes()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);

    let mut output = EventOutput::new(
        Capture::default(),
        Capture::default(),
        OutputOptions::default(),
    );
    output.write_all(&3_u32.to_le_bytes()).unwrap();
    let error = output.write_all(b"bad").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn headers_and_values_share_columns_across_different_length_rows() {
    let stdout = Capture::default();
    let mut output = EventOutput::new(stdout.clone(), Capture::default(), OutputOptions::default());
    let first = decode_event();
    output.event(&first).unwrap();
    let mut second = first.clone();
    second["snr_db"] = json!(-9);
    second["dt_seconds"] = json!(-0.62);
    second["frequency_hz"] = json!(452.1);
    output.event(&second).unwrap();
    let text = stdout.text();
    assert!(
        !text.contains('\t'),
        "terminal tab stops must not determine the columns: {text:?}"
    );
    let rows: Vec<_> = text.lines().collect();
    assert_eq!(rows.len(), 3);
    for (header, first, second) in [
        ("Mode", "FT8", "FT8"),
        ("SNR", "-12", "-9"),
        ("DT", "+0.00", "-0.62"),
        ("Freq", "1499.7", "452.1"),
        ("Message", "CQ K1ABC FN42", "CQ K1ABC FN42"),
    ] {
        let column = rows[0].find(header).unwrap();
        assert_eq!(rows[1].find(first), Some(column), "{header}: {text}");
        assert_eq!(rows[2].find(second), Some(column), "{header}: {text}");
    }
}

#[test]
fn long_custom_values_widen_columns_without_truncation() {
    let stdout = Capture::default();
    let mut output = EventOutput::new(
        stdout.clone(),
        Capture::default(),
        OutputOptions {
            fields: Some(vec!["message".into(), "snr".into()]),
            ..OutputOptions::default()
        },
    );
    output.event(&decode_event()).unwrap();
    let mut long = decode_event();
    let message = "A LONG MESSAGE THAT EXCEEDS THE INITIAL COLUMN WIDTH";
    long["message"] = json!(message);
    output.event(&long).unwrap();
    let text = stdout.text();
    let rows: Vec<_> = text.lines().collect();
    assert_eq!(rows.len(), 4, "a wider row needs an updated header");
    assert!(rows[3].starts_with(message));
    assert_eq!(rows[0].find("SNR"), rows[1].find("-12"));
    assert_eq!(rows[2].find("SNR"), rows[3].find("-12"));
    assert!(rows.iter().all(|row| !row.ends_with(' ')));
}

#[test]
fn slot_separators_group_decodes_and_include_empty_slots() {
    let stdout = Capture::default();
    let mut output = EventOutput::new(stdout.clone(), Capture::default(), OutputOptions::default());
    let mut event = decode_event();
    output.event(&event).unwrap();
    output.event(&event).unwrap();
    output
        .event(&json!({"type":"done", "status":"completed", "window_start_sample":"180000"}))
        .unwrap();
    // The next slot has no messages, but must still be visible.
    output
        .event(&json!({"type":"done", "status":"completed", "window_start_sample":"360000"}))
        .unwrap();
    event["window_start_sample"] = json!("540000");
    output.event(&event).unwrap();
    output
        .event(&json!({"type":"done", "status":"completed", "window_start_sample":"540000"}))
        .unwrap();
    let text = stdout.text();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 6, "{text}");
    assert!(lines[0].starts_with("Time"));
    assert!(lines[1].contains("CQ K1ABC FN42"));
    assert!(lines[2].contains("CQ K1ABC FN42"));
    assert_eq!(lines[3], "--- +00:00:30.000 ---");
    assert_eq!(lines[4], "--- +00:00:45.000 ---");
    assert!(lines[5].contains("CQ K1ABC FN42"));
}

#[test]
fn slot_separators_can_be_disabled_for_plain_message_output() {
    let stdout = Capture::default();
    let mut output = EventOutput::new(
        stdout.clone(),
        Capture::default(),
        OutputOptions {
            fields: Some(vec!["message".into()]),
            quiet: true,
            slot_separators: false,
            ..OutputOptions::default()
        },
    );
    let mut event = decode_event();
    output.event(&event).unwrap();
    event["window_start_sample"] = json!("270000");
    output.event(&event).unwrap();
    assert_eq!(stdout.text(), "CQ K1ABC FN42\nCQ K1ABC FN42\n");
}

#[test]
fn slot_separators_use_utc_in_quiet_custom_output() {
    let stdout = Capture::default();
    let mut output = EventOutput::new(
        stdout.clone(),
        Capture::default(),
        OutputOptions {
            fields: Some(vec!["message".into()]),
            quiet: true,
            ..OutputOptions::default()
        },
    );
    let mut event = decode_event();
    event["window_start_utc_ns"] = json!("0");
    output.event(&event).unwrap();
    event["window_start_sample"] = json!("270000");
    event["window_start_utc_ns"] = json!("7500000000");
    output.event(&event).unwrap();
    assert_eq!(
        stdout.text(),
        "CQ K1ABC FN42\n--- 00:00:07.500 ---\nCQ K1ABC FN42\n"
    );
}

#[test]
fn jsonl_has_no_slot_separators() {
    let stdout = Capture::default();
    let mut output = EventOutput::new(
        stdout.clone(),
        Capture::default(),
        OutputOptions {
            format: OutputFormat::Jsonl,
            ..OutputOptions::default()
        },
    );
    let first = decode_event();
    let mut second = first.clone();
    second["window_start_sample"] = json!("360000");
    output.event(&first).unwrap();
    output.event(&second).unwrap();
    let events: Vec<Value> = stdout
        .text()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events, [first, second]);
}

#[test]
fn default_text_identifies_ap_and_questionable_results() {
    let stdout = Capture::default();
    let mut output = EventOutput::new(stdout.clone(), Capture::default(), OutputOptions::default());
    let mut event = decode_event();
    event["assisted"] = json!(true);
    event["ap_type"] = json!(8);
    event["confidence"] = json!(0.16);
    event["questionable"] = json!(true);
    output.event(&event).unwrap();
    assert!(stdout.text().contains("a8?"), "{}", stdout.text());
}
