// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendering for WAV decode events.

use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use serde_json::Value;

const DEFAULT_FIELDS: &[&str] = &["time", "mode", "snr", "dt", "freq", "ap_type", "message"];
const ALLOWED_FIELDS: &[&str] = &[
    "time",
    "mode",
    "snr",
    "dt",
    "freq",
    "message",
    "type",
    "fec",
    "bp_iterations",
    "osd_pass",
    "hard_errors",
    "pass",
    "hashes",
    "payload",
    "assisted",
    "ap_type",
    "confidence",
    "questionable",
    "grid",
    "window_start_sample",
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputFormat {
    #[default]
    Text,
    Jsonl,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "text" => Ok(Self::Text),
            "jsonl" => Ok(Self::Jsonl),
            _ => Err("output format must be text or jsonl".into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputOptions {
    pub format: OutputFormat,
    pub verbosity: u8,
    pub fields: Option<Vec<String>>,
    pub stats: bool,
    pub quiet: bool,
    pub slot_separators: bool,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            format: OutputFormat::Text,
            verbosity: 0,
            fields: None,
            stats: false,
            quiet: false,
            slot_separators: true,
        }
    }
}

impl OutputOptions {
    pub fn parse_fields(value: &str) -> Result<Vec<String>, String> {
        if value.is_empty() {
            return Err("fields must not be empty".into());
        }
        let mut result = Vec::new();
        for field in value.split(',') {
            if field.is_empty() {
                return Err("field names must not be empty".into());
            }
            if !ALLOWED_FIELDS.contains(&field) {
                return Err(format!(
                    "unknown output field: {field}; supported fields: {}",
                    ALLOWED_FIELDS.join(", ")
                ));
            }
            if result.iter().any(|old| old == field) {
                return Err(format!("duplicate output field: {field}"));
            }
            result.push(field.to_owned());
        }
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.format == OutputFormat::Jsonl && self.fields.is_some() {
            return Err("--fields is available only with text output".into());
        }
        if let Some(fields) = &self.fields {
            let parsed = Self::parse_fields(&fields.join(","))?;
            if &parsed != fields {
                return Err("invalid output fields".into());
            }
        }
        Ok(())
    }

    pub fn text_fields(&self) -> Vec<String> {
        if let Some(fields) = &self.fields {
            return fields.clone();
        }
        let mut fields: Vec<String> = DEFAULT_FIELDS.iter().map(|field| (*field).into()).collect();
        if self.verbosity >= 1 {
            fields.extend(["type", "fec", "bp_iterations"].map(str::to_owned));
        }
        if self.verbosity >= 2 {
            fields.extend(
                [
                    "osd_pass",
                    "hard_errors",
                    "pass",
                    "confidence",
                    "questionable",
                    "hashes",
                    "payload",
                ]
                .map(str::to_owned),
            );
        }
        fields
    }

    pub fn diagnostics_level(&self) -> u8 {
        let fields = self.text_fields();
        if self.verbosity >= 2 || fields.iter().any(|field| field == "payload") {
            2
        } else if self.verbosity >= 1
            || fields.iter().any(|field| {
                matches!(
                    field.as_str(),
                    "type" | "grid" | "fec" | "bp_iterations" | "osd_pass" | "hard_errors" | "pass"
                )
            })
        {
            1
        } else {
            0
        }
    }

    pub fn window_stats_enabled(&self) -> bool {
        self.stats || self.verbosity > 0
    }
}

/// Incremental renderer for framed decoder events.
pub struct EventOutput<W: Write, D: Write> {
    writer: W,
    diagnostics: D,
    options: OutputOptions,
    errors: Arc<AtomicBool>,
    pending: Vec<u8>,
    header_len: Option<usize>,
    wrote_header: bool,
    column_widths: Vec<usize>,
    last_slot: Option<u128>,
    summary: Summary,
}

#[derive(Default)]
struct Summary {
    windows: u64,
    completed: u64,
    skipped: u64,
    cancelled: u64,
    failed: u64,
    messages: u64,
    candidates: u64,
    passes: u64,
    internal_decodes: u64,
    bp_decodes: u64,
    osd_decodes: u64,
    decode_ms: f64,
    converted_millis: Option<u128>,
    has_stats: bool,
}

impl<W: Write, D: Write> EventOutput<W, D> {
    pub fn new(writer: W, diagnostics: D, options: OutputOptions) -> Self {
        Self {
            writer,
            diagnostics,
            options,
            errors: Arc::new(AtomicBool::new(false)),
            pending: Vec::new(),
            header_len: None,
            wrote_header: false,
            column_widths: Vec::new(),
            last_slot: None,
            summary: Summary::default(),
        }
    }

    pub fn error_flag(&self) -> Arc<AtomicBool> {
        self.errors.clone()
    }

    pub fn event(&mut self, event: &Value) -> io::Result<()> {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "error" || (kind == "done" && event["status"] == "failed") {
            self.errors.store(true, Ordering::Relaxed);
        }
        if self.options.format == OutputFormat::Jsonl {
            serde_json::to_writer(&mut self.writer, event).map_err(invalid)?;
            self.writer.write_all(b"\n")?;
            return self.writer.flush();
        }

        if self.options.slot_separators
            && matches!(kind, "decode" | "done")
            && let Some(slot) = parse_integer(event.get("window_start_sample"))
            && self.last_slot != Some(slot)
        {
            if self.last_slot.is_some() {
                writeln!(self.writer, "--- {} ---", format_time(event))?;
                self.writer.flush()?;
            }
            self.last_slot = Some(slot);
        }

        match kind {
            "decode" => self.decode(event),
            "input" if !self.options.quiet => self.input(event),
            "done" => self.done(event),
            "warning" => self.diagnostic("Warning", event),
            "error" => self.diagnostic("Error", event),
            "finish" if !self.options.quiet => self.summary(),
            "ok" if event["command"] == "finish" && !self.options.quiet => self.summary(),
            _ => Ok(()),
        }
    }

    fn decode(&mut self, event: &Value) -> io::Result<()> {
        let fields = self.options.text_fields();
        let columns: Vec<_> = fields
            .iter()
            .map(|field| field_value(event, field))
            .collect();
        self.column_widths.resize(fields.len(), 0);
        // The final column needs no padding. Widen earlier columns when necessary
        // and repeat the header so streaming output never truncates long values.
        let mut widened = false;
        for (index, field) in fields
            .iter()
            .enumerate()
            .take(fields.len().saturating_sub(1))
        {
            let width = field_width(field)
                .max(field_header(field).chars().count())
                .max(columns[index].chars().count());
            if width > self.column_widths[index] {
                self.column_widths[index] = width;
                widened = true;
            }
        }
        if (!self.wrote_header || widened) && fields.as_slice() != ["message"] {
            let headers: Vec<_> = fields.iter().map(|field| field_header(field)).collect();
            write_columns(&mut self.writer, &headers, &self.column_widths)?;
            self.wrote_header = true;
        }
        write_columns(&mut self.writer, &columns, &self.column_widths)?;
        self.writer.flush()
    }

    fn input(&mut self, event: &Value) -> io::Result<()> {
        let rate = display(event.get("source_rate"));
        let channels = display(event.get("channels"));
        let channel_word = if integer(event.get("channels")) == 1 {
            "channel"
        } else {
            "channels"
        };
        let bits = display(event.get("bits_per_sample"));
        let format = display(event.get("sample_format"));
        let channel = display(event.get("channel"));
        let source = display(event.get("source_frames"));
        let output = display(event.get("output_frames"));
        let output_rate = display(event.get("output_rate"));
        self.summary.converted_millis = parse_integer(event.get("output_frames"))
            .map(|frames| frames.saturating_mul(1_000) / 12_000);
        writeln!(
            self.diagnostics,
            "Input: {rate} Hz, {channels} {channel_word}, {bits}-bit {format}, {channel}; {source} source frames → {output} at {output_rate} Hz"
        )
    }

    fn done(&mut self, event: &Value) -> io::Result<()> {
        self.summary.windows += 1;
        match event.get("status").and_then(Value::as_str) {
            Some("completed") => self.summary.completed += 1,
            Some("skipped") => self.summary.skipped += 1,
            Some("cancelled") => self.summary.cancelled += 1,
            Some("failed") => self.summary.failed += 1,
            _ => {}
        }
        self.summary.messages += integer(event.get("message_count"));
        let stats = &event["stats"];
        self.summary.has_stats |= stats.is_object();
        self.summary.candidates += integer(stats.get("candidates"));
        self.summary.passes += integer(stats.get("passes"));
        self.summary.internal_decodes += integer(stats.get("internal_decodes"));
        self.summary.bp_decodes += integer(stats.get("bp_decodes"));
        self.summary.osd_decodes += integer(stats.get("osd_decodes"));
        self.summary.decode_ms += number(stats.get("decode_ms"));

        if event["status"] == "failed" {
            return self.diagnostic("Error", event);
        }
        if self.options.quiet {
            return Ok(());
        }
        if let Some(status) = event.get("status").and_then(Value::as_str)
            && status != "completed"
        {
            writeln!(
                self.diagnostics,
                "{} window {}: {}",
                title(status),
                format_time(event),
                display(event.get("reason"))
            )?;
        }
        if !self.options.window_stats_enabled() {
            return Ok(());
        }
        writeln!(
            self.diagnostics,
            "Window {}: {} candidates, {} passes, {} internal, {} emitted ({} BP, {} OSD), {} ms, {}",
            format_time(event),
            display(stats.get("candidates")),
            display(stats.get("passes")),
            display(stats.get("internal_decodes")),
            display(event.get("message_count")),
            display(stats.get("bp_decodes")),
            display(stats.get("osd_decodes")),
            fixed(stats.get("decode_ms"), 2),
            display(event.get("status")),
        )
    }

    fn diagnostic(&mut self, label: &str, event: &Value) -> io::Result<()> {
        let message = event
            .get("message")
            .or_else(|| event.get("reason"))
            .map(value_text)
            .unwrap_or_else(|| "unknown failure".into());
        writeln!(self.diagnostics, "{label}: {message}")?;
        self.diagnostics.flush()
    }

    fn summary(&mut self) -> io::Result<()> {
        let duration = self
            .summary
            .converted_millis
            .map(format_duration)
            .unwrap_or_else(|| "—".into());
        write!(
            self.diagnostics,
            "Summary: {} {} ({} completed, {} skipped, {} cancelled, {} failed), {} {}",
            self.summary.windows,
            plural(self.summary.windows, "window", "windows"),
            self.summary.completed,
            self.summary.skipped,
            self.summary.cancelled,
            self.summary.failed,
            self.summary.messages,
            plural(self.summary.messages, "message", "messages"),
        )?;
        if self.summary.has_stats {
            write!(self.diagnostics, ", {} candidates", self.summary.candidates)?;
        }
        write!(self.diagnostics, ", {duration} converted")?;
        if self.summary.has_stats {
            write!(
                self.diagnostics,
                ", {:.2} ms decoder",
                self.summary.decode_ms
            )?;
        }
        writeln!(self.diagnostics)
    }
}

impl<W: Write, D: Write> Write for EventOutput<W, D> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut offset = 0;
        while offset < bytes.len() {
            let target = self.header_len.unwrap_or(4);
            let count = (target - self.pending.len()).min(bytes.len() - offset);
            self.pending
                .extend_from_slice(&bytes[offset..offset + count]);
            offset += count;
            if self.pending.len() != target {
                continue;
            }
            if self.header_len.is_none() {
                let length = u32::from_le_bytes(self.pending[..4].try_into().unwrap()) as usize;
                if length == 0 || length > crate::protocol::MAX_HEADER_BYTES {
                    return Err(invalid("invalid output header length"));
                }
                self.pending.clear();
                self.header_len = Some(length);
            } else {
                let event: Value = serde_json::from_slice(&self.pending).map_err(invalid)?;
                if event.get("payload_bytes").and_then(Value::as_u64) != Some(0) {
                    return Err(invalid("unexpected binary result"));
                }
                self.pending.clear();
                self.header_len = None;
                self.event(&event)?;
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.diagnostics.flush()
    }
}

fn invalid(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn plural<'a>(count: u64, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn title(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn format_duration(millis: u128) -> String {
    format!("{}.{:03} s", millis / 1_000, millis % 1_000)
}

fn integer(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(0.0)
}

fn display(value: Option<&Value>) -> String {
    value.map(value_text).unwrap_or_else(|| "—".into())
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Null => "—".into(),
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        other => other.to_string(),
    }
}

fn fixed(value: Option<&Value>, precision: usize) -> String {
    value
        .and_then(Value::as_f64)
        .map(|value| format!("{value:.precision$}"))
        .unwrap_or_else(|| "—".into())
}

fn write_columns(
    writer: &mut impl Write,
    columns: &[impl AsRef<str>],
    widths: &[usize],
) -> io::Result<()> {
    for (index, column) in columns.iter().enumerate() {
        let column = column.as_ref();
        if index + 1 == columns.len() {
            writeln!(writer, "{column}")?;
        } else {
            write!(writer, "{column:<width$}  ", width = widths[index])?;
        }
    }
    Ok(())
}

fn field_width(field: &str) -> usize {
    match field {
        "time" => 13,
        "mode" => 4,
        "ap_type" => 3,
        "snr" => 3,
        "dt" | "freq" => 6,
        "message" => 37,
        _ => 0,
    }
}

fn field_header(field: &str) -> &'static str {
    match field {
        "time" => "Time",
        "mode" => "Mode",
        "snr" => "SNR",
        "dt" => "DT",
        "freq" => "Freq",
        "message" => "Message",
        "type" => "Type",
        "fec" => "FEC",
        "bp_iterations" => "BP iterations",
        "osd_pass" => "OSD pass",
        "hard_errors" => "Hard errors",
        "pass" => "Pass",
        "hashes" => "Hashes",
        "payload" => "Payload",
        "assisted" => "Assisted",
        "ap_type" => "AP",
        "confidence" => "Confidence",
        "questionable" => "Questionable",
        "grid" => "Grid",
        "window_start_sample" => "Window start sample",
        _ => "Unknown",
    }
}

fn field_value(event: &Value, field: &str) -> String {
    match field {
        "time" => format_time(event),
        "mode" => event
            .get("mode")
            .and_then(Value::as_str)
            .map(str::to_uppercase)
            .unwrap_or_else(|| "—".into()),
        "snr" => display(event.get("snr_db")),
        "dt" => event
            .get("dt_seconds")
            .and_then(Value::as_f64)
            .map(|value| {
                let value = if value.abs() < 0.005 { 0.0 } else { value };
                format!("{value:+.2}")
            })
            .unwrap_or_else(|| "—".into()),
        "freq" => fixed(event.get("frequency_hz"), 1),
        "message" => display(event.get("message")),
        "ap_type" => event
            .get("ap_type")
            .and_then(Value::as_u64)
            .map(|kind| {
                format!(
                    "a{kind}{}",
                    if event["questionable"] == true {
                        "?"
                    } else {
                        ""
                    }
                )
            })
            .unwrap_or_default(),
        "confidence" => fixed(event.get("confidence"), 2),
        "type" => display(event.get("message_type")),
        "fec" | "bp_iterations" | "osd_pass" | "hard_errors" | "pass" => {
            display(event.get("diagnostics").and_then(|value| value.get(field)))
        }
        "hashes" => format_hashes(event.get("hashes")),
        "payload" | "assisted" | "questionable" | "grid" | "window_start_sample" => {
            display(event.get(field))
        }
        _ => "—".into(),
    }
}

fn format_hashes(value: Option<&Value>) -> String {
    let Some(values) = value.and_then(Value::as_array) else {
        return "—".into();
    };
    if values.is_empty() {
        return "—".into();
    }
    values
        .iter()
        .map(|hash| {
            format!(
                "{}:{}:{}",
                display(hash.get("width")),
                display(hash.get("value")),
                display(hash.get("resolved"))
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_integer(value: Option<&Value>) -> Option<u128> {
    value.and_then(|value| match value {
        Value::String(value) => value.parse().ok(),
        Value::Number(value) => value.as_u64().map(u128::from),
        _ => None,
    })
}

fn format_time(event: &Value) -> String {
    if let Some(ns) = parse_integer(event.get("window_start_utc_ns")) {
        let millis = ns / 1_000_000 % 86_400_000;
        return format_clock(millis, false);
    }
    let Some(samples) = parse_integer(event.get("window_start_sample")) else {
        return "—".into();
    };
    format_clock(samples.saturating_mul(1_000) / 12_000, true)
}

fn format_clock(total_millis: u128, relative: bool) -> String {
    let hours = total_millis / 3_600_000;
    let minutes = total_millis / 60_000 % 60;
    let seconds = total_millis / 1_000 % 60;
    let millis = total_millis % 1_000;
    format!(
        "{}{:02}:{minutes:02}:{seconds:02}.{millis:03}",
        if relative { "+" } else { "" },
        hours
    )
}
