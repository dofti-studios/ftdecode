// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded stream assembly and an ordered, full-duplex decode worker.
use crate::engine::{DecodeSettings, DecodeStats, DecodedSignal, Decoder, Mode};
use crate::protocol::{self, Frame};
use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender, TrySendError},
};
use std::time::{Duration, Instant};

type Output = (Vec<u8>, mpsc::Sender<io::Result<()>>);
struct TimedWriter {
    tx: SyncSender<Output>,
    bytes: Vec<u8>,
    live: Arc<AtomicBool>,
}
impl Write for TimedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len() + bytes.len() > protocol::MAX_HEADER_BYTES + 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "output frame too large",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.bytes.is_empty() {
            return Ok(());
        }
        let (ack, result) = mpsc::channel();
        self.tx
            .send((std::mem::take(&mut self.bytes), ack))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "output closed"))?;
        if self.live.load(Ordering::Acquire) {
            result.recv_timeout(Duration::from_secs(5)).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "live consumer did not accept output within 5 seconds",
                )
            })?
        } else {
            result
                .recv()
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "output closed"))?
        }
    }
}

/// Implemented by the real decoder; deterministic transport tests inject a backend.
pub trait Backend {
    fn set_slot(&mut self, _slot: u64) {}
    fn reset(&mut self, mode: Mode);
    fn decode(
        &mut self,
        pcm: &[f32],
        settings: &DecodeSettings,
        cancel: &AtomicBool,
        on_decode: &mut dyn FnMut(DecodedSignal),
    ) -> Result<DecodeStats, String>;
}
impl Backend for Decoder {
    fn set_slot(&mut self, slot: u64) {
        Decoder::set_slot(self, slot);
    }
    fn reset(&mut self, mode: Mode) {
        Decoder::reset(self, mode);
    }
    fn decode(
        &mut self,
        pcm: &[f32],
        settings: &DecodeSettings,
        cancel: &AtomicBool,
        on_decode: &mut dyn FnMut(DecodedSignal),
    ) -> Result<DecodeStats, String> {
        Decoder::decode(self, pcm, settings, cancel, on_decode)
    }
}

pub fn signal_json(
    signal: &DecodedSignal,
    mode: Mode,
    window_start: u64,
    utc: Option<u64>,
) -> Value {
    let hash_json = |h: &crate::message::CallsignHash| json!({"width":h.width,"value":h.value,"resolved":h.resolved});
    let hashes: Vec<_> = signal.message.hashes.iter().map(hash_json).collect();
    let fields = &signal.message.fields;
    let sender = fields.sender.as_ref();
    let recipient = fields.recipient.as_ref();
    let mut value = json!({"type":"decode","message":signal.message.text,
        "message_type":signal.message.kind.name(),
        "exchange_type":fields.exchange_type.map(crate::message::ExchangeType::name),
        "sender":sender.and_then(|s| s.call.as_deref()),
        "recipient":recipient.and_then(|s| s.call.as_deref()),
        "sender_hash":sender.and_then(|s| s.hash.as_ref()).map(hash_json),
        "recipient_hash":recipient.and_then(|s| s.hash.as_ref()).map(hash_json),
        "cq_modifier":fields.cq_modifier,"grid":signal.message.grid,"report_db":fields.report_db,
        "hashes":hashes,"mode":mode.name(),"frequency_hz":signal.frequency_hz,
        "snr_db":signal.snr_db,"dt_seconds":signal.dt_seconds,
        "window_start_sample":window_start.to_string(),"assisted":signal.assisted});
    if let Some(diagnostics) = &signal.diagnostics
        && let Some(ap_type) = diagnostics.ap_type
    {
        value["ap_type"] = json!(ap_type);
        value["confidence"] = json!(diagnostics.confidence);
        value["questionable"] = json!(diagnostics.questionable);
    }
    if let Some(utc) = utc {
        value["window_start_utc_ns"] = json!(utc.to_string());
    }
    value
}

/// WAV-only diagnostics layered on the common decode metadata.
fn detailed_signal_json(
    signal: &DecodedSignal,
    mode: Mode,
    start: u64,
    utc: Option<u64>,
    level: u8,
) -> Value {
    let mut value = signal_json(signal, mode, start, utc);
    if level > 0 {
        value["diagnostics"] = signal.diagnostics.as_ref().map(|d| json!({
            "fec": match d.method { crate::fec::DecodeMethod::Bp => "BP", crate::fec::DecodeMethod::Osd => "OSD", crate::fec::DecodeMethod::List => "list" },
            "bp_iterations":d.bp_iterations,"osd_pass":d.osd_pass,"hard_errors":d.hard_errors,"pass":d.pass,
        })).unwrap_or(Value::Null);
    }
    if level > 1 {
        value["payload"] = json!(
            signal
                .source_bits
                .iter()
                .map(|b| if *b { '1' } else { '0' })
                .collect::<String>()
        );
    }
    value
}

/// Options for the in-process converted WAV path, not accepted over TCP/stdin.
#[derive(Clone, Copy, Default)]
pub struct WavOptions {
    pub diagnostics: u8,
    pub stats: bool,
}

#[derive(Default)]
struct WindowStatistics {
    candidates: usize,
    passes: usize,
    internal_decodes: usize,
    elapsed: Duration,
}

struct Window {
    mode: Mode,
    start: u64,
    utc: Option<u64>,
    pcm: Vec<f32>,
    settings: DecodeSettings,
    cancel: Arc<AtomicBool>,
    skipped: Vec<(u64, u64)>,
    final_pass: bool,
}
struct Published {
    start: u64,
    cancel: Arc<AtomicBool>,
    signals: Vec<DecodedSignal>,
    stats: Option<WindowStatistics>,
    failure: Option<String>,
    mode: Mode,
    utc: Option<u64>,
}
fn terminal<W: Write>(
    writer: &mut W,
    mut event: Value,
    published: &mut Option<Published>,
    context: Option<(Mode, Option<u64>, bool)>,
) -> io::Result<()> {
    // Undecoded windows (short tails, gaps, overload) still have a known
    // timeline and performed zero work. Only the WAV adapter requests this.
    if event["type"] == "done"
        && let Some((mode, utc, stats)) = context
    {
        event["mode"] = json!(mode.name());
        if let Some(utc) = utc {
            let sample = event["window_start_sample"]
                .as_str()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid terminal sample position",
                    )
                })?;
            let time = u64::try_from(u128::from(utc) + u128::from(sample) * 1_000_000_000 / 12000)
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "window UTC exceeds u64")
                })?;
            event["window_start_utc_ns"] = json!(time.to_string());
        }
        if stats {
            event["stats"] = json!({"candidates":0,"passes":0,"internal_decodes":0,
                    "bp_decodes":0,"osd_decodes":0,"decode_ms":0.0});
        }
    }
    if event["type"] == "done"
        && published
            .as_ref()
            .is_some_and(|p| event["window_start_sample"].as_str() == Some(&p.start.to_string()))
    {
        let state = published.take().unwrap();
        event["message_count"] = json!(state.signals.len());
        if let Some(reason) = state.failure {
            event["status"] = json!("failed");
            event["reason"] = json!(reason);
        }
        if let Some(stats) = state.stats {
            use crate::fec::DecodeMethod;
            let count = |method| {
                state
                    .signals
                    .iter()
                    .filter(|s| s.diagnostics.as_ref().is_some_and(|d| d.method == method))
                    .count()
            };
            event["mode"] = json!(state.mode.name());
            if let Some(utc) = state.utc {
                event["window_start_utc_ns"] = json!(utc.to_string());
            }
            event["stats"] = json!({"candidates":stats.candidates,"passes":stats.passes,
                "internal_decodes":stats.internal_decodes,"bp_decodes":count(DecodeMethod::Bp),
                "osd_decodes":count(DecodeMethod::Osd),"decode_ms":stats.elapsed.as_secs_f64()*1000.0});
        }
    }
    protocol::write_frame(writer, &event)
}
type PendingWindow = Arc<Mutex<Option<Window>>>;
enum Work {
    Window(PendingWindow),
    Event(Value),
    Fatal(io::Error),
    Reset(Mode, &'static str, Option<u64>),
    Finish,
}
fn done(start: u64, status: &str, count: usize, reason: Option<&str>) -> Value {
    let mut v = json!({"type":"done","window_start_sample":start.to_string(),
        "status":status,"message_count":count});
    if let Some(reason) = reason {
        v["reason"] = json!(reason);
    }
    v
}
fn error(command: &str, message: &str) -> Value {
    json!({"type":"error","command":command,"message":message})
}
fn send(tx: &SyncSender<Work>, work: Work) -> Result<(), String> {
    tx.send(work).map_err(|_| "output connection closed".into())
}

/// The caller must arrange interruptible writes (TCP uses a write timeout).
/// A blocked input reader is detached on output failure; TCP callers should
/// shut down a retained socket clone when this function returns.
pub fn serve<R: Read + Send + 'static, W: Write + Send + 'static>(
    reader: R,
    writer: W,
) -> io::Result<()> {
    serve_with_decoder(reader, writer, Decoder::new(Mode::Ft8))
}

pub fn serve_with_decoder<R: Read + Send + 'static, W: Write + Send + 'static, B: Backend>(
    reader: R,
    writer: W,
    backend: B,
) -> io::Result<()> {
    serve_internal(reader, writer, backend, false, WavOptions::default())
}

/// Consumes trusted in-process frames containing PCM-scale f32 audio.
/// This is deliberately separate from the public s16le wire protocol.
pub fn serve_wav<R: Read + Send + 'static, W: Write + Send + 'static>(
    reader: R,
    writer: W,
    options: WavOptions,
) -> io::Result<()> {
    serve_wav_with_decoder(reader, writer, Decoder::new_for_wav(Mode::Ft8), options)
}

#[doc(hidden)]
pub fn serve_wav_with_decoder<R: Read + Send + 'static, W: Write + Send + 'static, B: Backend>(
    reader: R,
    writer: W,
    backend: B,
    options: WavOptions,
) -> io::Result<()> {
    serve_internal(reader, writer, backend, true, options)
}

fn serve_internal<R: Read + Send + 'static, W: Write + Send + 'static, B: Backend>(
    reader: R,
    mut writer: W,
    mut backend: B,
    float_pcm: bool,
    options: WavOptions,
) -> io::Result<()> {
    // Two pending windows plus one active and one being assembled. No output
    // queue: callbacks write synchronously and apply backpressure to the worker.
    let (tx, rx) = mpsc::sync_channel(2);
    let closed = Arc::new(AtomicBool::new(false));
    let live = Arc::new(AtomicBool::new(false));
    let reader_live = live.clone();
    let (out_tx, out_rx) = mpsc::sync_channel::<Output>(1);
    std::thread::spawn(move || {
        for (bytes, ack) in out_rx {
            let result = writer.write_all(&bytes).and_then(|_| writer.flush());
            let failed = result.is_err();
            let _ = ack.send(result);
            if failed {
                break;
            }
        }
    });
    let mut writer = TimedWriter {
        tx: out_tx,
        bytes: Vec::new(),
        live,
    };
    let reader_closed = closed.clone();
    std::thread::spawn(move || ingress(reader, tx, reader_closed, reader_live, float_pcm));
    let result = (|| {
        let mut published: Option<Published> = None;
        let mut context = None;
        for work in rx {
            match work {
                Work::Event(event) => terminal(&mut writer, event, &mut published, context)?,
                Work::Fatal(e) => {
                    protocol::write_frame(&mut writer, &error("frame", &e.to_string()))?;
                    return Err(e);
                }
                Work::Reset(mode, command, utc) => {
                    published = None;
                    context = float_pcm.then_some((mode, utc, options.stats));
                    backend.reset(mode);
                    protocol::write_frame(&mut writer, &json!({"type":"ok","command":command}))?;
                }
                Work::Finish => {
                    protocol::write_frame(&mut writer, &json!({"type":"ok","command":"finish"}))?;
                    break;
                }
                Work::Window(pending) => {
                    let window = pending.lock().unwrap().take().unwrap();
                    for &(first, last) in &window.skipped {
                        protocol::write_frame(
                            &mut writer,
                            &json!({"type":"warning","reason":"overload",
                            "first_sample":first.to_string(),"end_sample":(last+window.mode.input_samples() as u64).to_string()}),
                        )?;
                        let mut start = first;
                        loop {
                            terminal(
                                &mut writer,
                                done(start, "skipped", 0, Some("obsolete live work")),
                                &mut published,
                                context,
                            )?;
                            if start == last {
                                break;
                            }
                            start += window.mode.period_samples() as u64;
                        }
                    }
                    if window.cancel.load(Ordering::Acquire) {
                        if window.final_pass {
                            terminal(
                                &mut writer,
                                done(window.start, "cancelled", 0, Some("reset or disconnect")),
                                &mut published,
                                context,
                            )?;
                        }
                        continue;
                    }
                    if !published.as_ref().is_some_and(|p| {
                        p.start == window.start && Arc::ptr_eq(&p.cancel, &window.cancel)
                    }) {
                        published = Some(Published {
                            start: window.start,
                            cancel: window.cancel.clone(),
                            signals: Vec::new(),
                            stats: options.stats.then(WindowStatistics::default),
                            failure: None,
                            mode: window.mode,
                            utc: window.utc,
                        });
                    }
                    if published.as_ref().unwrap().failure.is_some() {
                        if window.final_pass {
                            terminal(
                                &mut writer,
                                done(window.start, "failed", 0, None),
                                &mut published,
                                context,
                            )?;
                        }
                        continue;
                    }
                    let state = published.as_mut().unwrap();
                    let signals = &mut state.signals;
                    let mut output_error = None;
                    let mut pcm = window.pcm;
                    let mut output_elapsed = Duration::ZERO;
                    let started = Instant::now();
                    pcm.resize(window.mode.input_samples(), 0.0);
                    backend.set_slot(
                        window.utc.map_or(
                            window.start / window.mode.period_samples() as u64,
                            |utc| {
                                utc / (window.mode.period_samples() as u64 * 1_000_000_000 / 12000)
                            },
                        ),
                    );
                    let result =
                        backend.decode(&pcm, &window.settings, &window.cancel, &mut |signal| {
                            if output_error.is_some() || window.cancel.load(Ordering::Acquire) {
                                return;
                            }
                            if signals.iter().any(|old| {
                                old.source_bits == signal.source_bits
                                    && (old.frequency_hz - signal.frequency_hz).abs() < 4.0
                                    && (old.dt_seconds - signal.dt_seconds).abs() < 0.08
                            }) {
                                return;
                            }
                            if signals.len() >= 4096 {
                                window.cancel.store(true, Ordering::Release);
                                output_error = Some(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "decoder exceeded 4096 unique signals per window",
                                ));
                                return;
                            }
                            let output_started = Instant::now();
                            match protocol::write_frame(
                                &mut writer,
                                &detailed_signal_json(
                                    &signal,
                                    window.mode,
                                    window.start,
                                    window.utc,
                                    options.diagnostics,
                                ),
                            ) {
                                Ok(()) => signals.push(signal),
                                Err(e) => {
                                    window.cancel.store(true, Ordering::Release);
                                    output_error = Some(e);
                                }
                            }
                            output_elapsed += output_started.elapsed();
                        });
                    if let Some(stats) = &mut state.stats {
                        stats.elapsed += started.elapsed().saturating_sub(output_elapsed);
                        if let Ok(result) = &result {
                            stats.candidates += result.candidates;
                            stats.passes += result.passes;
                            stats.internal_decodes += result.decoded;
                        }
                    }
                    if let Some(e) = output_error {
                        return Err(e);
                    }
                    if let Err(reason) = &result {
                        state.failure = Some(reason.clone());
                    }
                    if !window.final_pass {
                        continue;
                    }
                    let count = signals.len();
                    let event = match result {
                        Ok(stats) if stats.cancelled || window.cancel.load(Ordering::Acquire) => {
                            done(
                                window.start,
                                "cancelled",
                                count,
                                Some("reset or disconnect"),
                            )
                        }
                        Ok(_) => done(window.start, "completed", count, None),
                        Err(reason) => done(window.start, "failed", count, Some(&reason)),
                    };
                    terminal(&mut writer, event, &mut published, context)?;
                }
            }
        }
        Ok(())
    })();
    closed.store(true, Ordering::Release);
    result
}

struct Session {
    mode: Mode,
    live: bool,
    utc: Option<u64>,
    next: u64,
    expected: u64,
    settings: DecodeSettings,
    opened_settings: Option<DecodeSettings>,
    pcm: Vec<f32>,
    cancel: Arc<AtomicBool>,
    pending: Weak<Mutex<Option<Window>>>,
    early_submitted: bool,
    cancellations: Vec<Weak<AtomicBool>>,
}

fn fields(v: &Value, allowed: &[&str]) -> Result<(), String> {
    let object = v.as_object().ok_or("expected a JSON object")?;
    if let Some(key) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!("unknown field {key}"));
    }
    Ok(())
}
fn decimal(v: &Value, name: &str) -> Result<u64, String> {
    let s = v
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} must be a decimal string"))?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{name} must be a nonnegative decimal string"));
    }
    s.parse().map_err(|_| format!("{name} exceeds u64"))
}
fn mode(v: &Value) -> Result<Mode, String> {
    match v.as_str() {
        Some("ft8") => Ok(Mode::Ft8),
        Some("ft4") => Ok(Mode::Ft4),
        _ => Err("mode must be ft8 or ft4".into()),
    }
}
fn settings(v: &Value, old: &DecodeSettings) -> Result<DecodeSettings, String> {
    fields(
        v,
        &[
            "low_hz",
            "high_hz",
            "priority_hz",
            "priority_tolerance_hz",
            "depth",
            "threads",
            "assistance",
            "ap",
            "rx_hz",
            "ap_width_hz",
            "tx_hz",
            "my_call",
            "dx_call",
            "dx_grid",
            "qso_state",
            "activity",
            "known_calls",
        ],
    )?;
    let mut s = old.clone();
    for (key, target) in [
        ("low_hz", &mut s.low_hz),
        ("high_hz", &mut s.high_hz),
        ("priority_tolerance_hz", &mut s.priority_tolerance_hz),
    ] {
        if let Some(v) = v.get(key) {
            *target = v.as_f64().ok_or_else(|| format!("{key} must be numeric"))? as f32;
        }
    }
    if let Some(v) = v.get("priority_hz") {
        s.priority_hz = if v.is_null() {
            None
        } else {
            Some(v.as_f64().ok_or("priority_hz must be numeric or null")? as f32)
        };
    }
    if let Some(v) = v.get("depth") {
        s.depth = v
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .ok_or("depth must be an integer in 1..3")?;
    }
    if let Some(v) = v.get("threads") {
        s.threads = v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or("threads must be an integer")?;
    }
    if let Some(v) = v.get("assistance") {
        s.assistance = v.as_bool().ok_or("assistance must be boolean")?;
    }
    if let Some(value) = v.get("rx_hz") {
        if v.get("priority_hz").is_some() {
            return Err("choose rx_hz or priority_hz, not both".into());
        }
        s.priority_hz = if value.is_null() {
            None
        } else {
            Some(value.as_f64().ok_or("rx_hz must be numeric or null")? as f32)
        };
    }
    if let Some(value) = v.get("ap") {
        s.ap.mode = value
            .as_str()
            .ok_or("ap must be off, cq or auto")?
            .parse()?;
        let enabled = s.ap.mode != crate::assistance::ApMode::Off;
        if v.get("assistance").is_some() && s.assistance != enabled {
            return Err("ap and assistance disagree".into());
        }
        s.assistance = enabled;
    } else if v.get("assistance").is_some() {
        s.ap.mode = if s.assistance {
            crate::assistance::ApMode::Auto
        } else {
            crate::assistance::ApMode::Off
        };
    }
    for (key, target) in [
        ("my_call", &mut s.ap.my_call),
        ("dx_call", &mut s.ap.dx_call),
        ("dx_grid", &mut s.ap.dx_grid),
    ] {
        if let Some(value) = v.get(key) {
            *target = if value.is_null() {
                None
            } else {
                Some(
                    value
                        .as_str()
                        .ok_or_else(|| format!("{key} must be a string or null"))?
                        .to_ascii_uppercase(),
                )
            };
        }
    }
    if let Some(value) = v.get("ap_width_hz") {
        s.ap.width_hz = value.as_f64().ok_or("ap_width_hz must be numeric")? as f32;
    }
    if let Some(value) = v.get("tx_hz") {
        s.ap.tx_hz = if value.is_null() {
            None
        } else {
            Some(value.as_f64().ok_or("tx_hz must be numeric or null")? as f32)
        };
    }
    if let Some(value) = v.get("qso_state") {
        s.ap.state = value
            .as_str()
            .ok_or("qso_state must be a string")?
            .parse()?;
    }
    if let Some(value) = v.get("activity") {
        s.ap.activity = value.as_str().ok_or("activity must be a string")?.parse()?;
    }
    if let Some(value) = v.get("known_calls") {
        let calls = value.as_array().ok_or("known_calls must be an array")?;
        if calls.len() > 400 {
            return Err("known_calls is limited to 400 entries".into());
        }
        s.ap.known_calls = calls
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|call| call.trim().to_ascii_uppercase())
                    .ok_or("known_calls must contain strings".to_owned())
            })
            .collect::<Result<_, _>>()?;
    }
    s.validate()?;
    Ok(s)
}
fn validate_mode_settings(mode: Mode, settings: &DecodeSettings) -> Result<(), String> {
    if mode == Mode::Ft4 && settings.threads != 1 {
        return Err("FT4 supports threads=1".into());
    }
    Ok(())
}
impl Session {
    fn new(v: &Value, previous: Option<&Self>) -> Result<Self, String> {
        let start = previous.is_none();
        fields(
            v,
            if start {
                &[
                    "type",
                    "payload_bytes",
                    "version",
                    "mode",
                    "operation",
                    "start_utc_ns",
                    "slot_offset_samples",
                    "settings",
                ]
            } else {
                &[
                    "type",
                    "payload_bytes",
                    "mode",
                    "start_utc_ns",
                    "slot_offset_samples",
                    "settings",
                ]
            },
        )?;
        if start && v.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("version must be 1".into());
        }
        let mode = match v.get("mode") {
            Some(v) => mode(v)?,
            None => previous.map(|s| s.mode).ok_or("start requires mode")?,
        };
        let live = if start {
            match v.get("operation").and_then(Value::as_str) {
                Some("live") => true,
                Some("offline") => false,
                _ => return Err("operation must be live or offline".into()),
            }
        } else {
            previous.unwrap().live
        };
        let (utc, next) = match (v.get("start_utc_ns"), v.get("slot_offset_samples")) {
            (Some(_), None) => {
                let utc = decimal(v, "start_utc_ns")?;
                let period_ns = mode.period_samples() as u64 * 1_000_000_000 / 12000;
                let remainder = utc % period_ns;
                let delta = if remainder == 0 {
                    0
                } else {
                    period_ns - remainder
                };
                (
                    Some(utc),
                    (delta as u128 * 12000).div_ceil(1_000_000_000) as u64,
                )
            }
            (None, Some(_)) => (None, decimal(v, "slot_offset_samples")?),
            _ => return Err("provide exactly one of start_utc_ns or slot_offset_samples".into()),
        };
        let old = previous.map(|s| s.settings.clone()).unwrap_or_default();
        let settings = match v.get("settings") {
            Some(v) => settings(v, &old)?,
            None => old,
        };
        validate_mode_settings(mode, &settings)?;
        let cancel = Arc::new(AtomicBool::new(false));
        Ok(Self {
            mode,
            live,
            utc,
            next,
            expected: 0,
            settings,
            opened_settings: None,
            pcm: Vec::new(),
            cancellations: vec![Arc::downgrade(&cancel)],
            cancel,
            pending: Weak::new(),
            early_submitted: false,
        })
    }
    fn renew_cancel(&mut self) {
        self.cancellations.retain(|flag| flag.strong_count() > 0);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.cancellations.push(Arc::downgrade(&self.cancel));
    }
    fn cancel_all(&self) {
        for flag in &self.cancellations {
            if let Some(flag) = flag.upgrade() {
                flag.store(true, Ordering::Release);
            }
        }
    }
    fn window_utc(&self) -> Result<Option<u64>, String> {
        self.utc
            .map(|base| {
                u64::try_from(base as u128 + self.next as u128 * 1_000_000_000 / 12000)
                    .map_err(|_| "window UTC exceeds u64".into())
            })
            .transpose()
    }
    fn audio(
        &mut self,
        frame: Frame,
        tx: &SyncSender<Work>,
        float_pcm: bool,
    ) -> Result<(), String> {
        let v = &frame.header;
        fields(
            v,
            &["type", "payload_bytes", "first_sample", "sample_count"],
        )?;
        let first = decimal(v, "first_sample")?;
        let count = v
            .get("sample_count")
            .and_then(Value::as_u64)
            .ok_or("sample_count must be an integer")?;
        let sample_bytes = if float_pcm { 4 } else { 2 };
        if count == 0 || count > 12000 || count as usize * sample_bytes != frame.payload.len() {
            return Err("sample_count must match 1..12000 PCM samples".into());
        }
        let samples: Vec<f32> = if float_pcm {
            frame
                .payload
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect()
        } else {
            frame
                .payload
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| f32::from(i16::from_le_bytes(*b)))
                .collect()
        };
        if samples
            .iter()
            .any(|x| !x.is_finite() || x.abs() > crate::engine::MAX_FILTERED_PCM)
        {
            return Err("converted audio must contain finite bounded PCM-scale samples".into());
        }
        let end = first
            .checked_add(count)
            .ok_or("audio sample range exceeds u64")?;
        if first < self.expected {
            return Err("audio overlaps or precedes previously received samples".into());
        }
        if first > self.expected {
            if first > self.next {
                self.cancel.store(true, Ordering::Release);
                self.renew_cancel();
            }
            send(
                tx,
                Work::Event(json!({"type":"warning","reason":"input_gap",
                "first_sample":self.expected.to_string(),"end_sample":first.to_string()})),
            )?;
            // Missing data before a future relative boundary does not damage it.
            if first > self.next {
                if self.opened_settings.is_some() {
                    send(
                        tx,
                        Work::Event(done(self.next, "skipped", 0, Some("input gap"))),
                    )?;
                }
                self.pcm.clear();
                self.opened_settings = None;
                self.early_submitted = false;
                let period = self.mode.period_samples() as u64;
                let periods = (first - self.next).div_ceil(period);
                self.next = self
                    .next
                    .checked_add(
                        periods
                            .checked_mul(period)
                            .ok_or("sample boundary exceeds u64")?,
                    )
                    .ok_or("sample boundary exceeds u64")?;
            }
        }
        self.expected = end;
        let mut position = first;
        while position < end {
            if position < self.next {
                position = end.min(self.next);
                continue;
            }
            if self.opened_settings.is_none() {
                self.opened_settings = Some(self.settings.clone());
            }
            let early = self.live
                && self.mode == Mode::Ft8
                && !self.early_submitted
                && self.opened_settings.as_ref().unwrap().depth >= 2;
            let target = if early {
                162432
            } else {
                self.mode.input_samples()
            };
            let needed = target - self.pcm.len();
            let take = needed.min((end - position) as usize);
            let offset = (position - first) as usize;
            self.pcm.extend_from_slice(&samples[offset..offset + take]);
            position += take as u64;
            if self.pcm.len() == target {
                let window = Window {
                    mode: self.mode,
                    start: self.next,
                    utc: self.window_utc()?,
                    pcm: if early {
                        self.pcm.clone()
                    } else {
                        std::mem::take(&mut self.pcm)
                    },
                    settings: if early {
                        self.opened_settings.clone().unwrap()
                    } else {
                        self.opened_settings.take().unwrap()
                    },
                    cancel: self.cancel.clone(),
                    skipped: Vec::new(),
                    final_pass: !early,
                };
                let pending = Arc::new(Mutex::new(Some(window)));
                if self.live {
                    match tx.try_send(Work::Window(pending.clone())) {
                        Ok(()) => self.pending = Arc::downgrade(&pending),
                        Err(TrySendError::Full(_)) if early => {}
                        Err(TrySendError::Full(_)) => {
                            // Replace obsolete pending PCM without dropping its
                            // terminal event. Consecutive skipped slots occupy
                            // one range; cap disjoint ranges to bound metadata.
                            let mut replaced = false;
                            if let Some(previous) = self.pending.upgrade() {
                                let mut slot = previous.lock().unwrap();
                                if let Some(old) = slot.as_mut() {
                                    let contiguous =
                                        old.skipped.last().is_some_and(|&(_, last)| {
                                            last.checked_add(self.mode.period_samples() as u64)
                                                == Some(old.start)
                                        });
                                    if contiguous || old.skipped.len() < 16 {
                                        let mut newest = pending.lock().unwrap().take().unwrap();
                                        newest.skipped = std::mem::take(&mut old.skipped);
                                        if !old.final_pass {
                                            // The final pass owns this window's
                                            // terminal event; an omitted early
                                            // attempt is not a skipped window.
                                        } else if contiguous {
                                            newest.skipped.last_mut().unwrap().1 = old.start;
                                        } else {
                                            newest.skipped.push((old.start, old.start));
                                        }
                                        *slot = Some(newest);
                                        replaced = true;
                                    }
                                }
                            }
                            if !replaced {
                                send(tx, Work::Window(pending.clone()))?;
                                self.pending = Arc::downgrade(&pending);
                            }
                        }
                        Err(_) => return Err("output connection closed".into()),
                    }
                } else {
                    send(tx, Work::Window(pending))?;
                }
                if early {
                    self.early_submitted = true;
                } else {
                    self.early_submitted = false;
                    self.renew_cancel();
                    self.next = self
                        .next
                        .checked_add(self.mode.period_samples() as u64)
                        .ok_or("sample boundary exceeds u64")?;
                }
            }
        }
        Ok(())
    }
}

fn ingress<R: Read>(
    mut reader: R,
    tx: SyncSender<Work>,
    closed: Arc<AtomicBool>,
    live: Arc<AtomicBool>,
    float_pcm: bool,
) {
    let mut session: Option<Session> = None;
    while !closed.load(Ordering::Acquire) {
        let frame = match protocol::read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(e) => {
                if let Some(session) = session.as_ref() {
                    session.cancel_all();
                }
                let _ = send(&tx, Work::Fatal(e));
                break;
            }
        };
        let command = frame.header["type"].as_str().unwrap_or("").to_owned();
        let result: Result<bool, String> = (|| {
            match command.as_str() {
                "start" => {
                    if session.is_some() {
                        return Err("stream already started; use reset".into());
                    }
                    let new = Session::new(&frame.header, None)?;
                    live.store(new.live, Ordering::Release);
                    send(&tx, Work::Reset(new.mode, "start", new.utc))?;
                    session = Some(new);
                }
                "reset" => {
                    let old = session.as_ref().ok_or("send start first")?;
                    let new = Session::new(&frame.header, Some(old))?;
                    old.cancel_all();
                    if old.opened_settings.is_some() {
                        send(
                            &tx,
                            Work::Event(done(old.next, "cancelled", 0, Some("reset"))),
                        )?;
                    }
                    send(&tx, Work::Reset(new.mode, "reset", new.utc))?;
                    session = Some(new);
                }
                "configure" => {
                    fields(&frame.header, &["type", "payload_bytes", "settings"])?;
                    let current = session.as_mut().ok_or("send start first")?;
                    let updated = settings(
                        frame
                            .header
                            .get("settings")
                            .ok_or("configure requires settings")?,
                        &current.settings,
                    )?;
                    validate_mode_settings(current.mode, &updated)?;
                    current.settings = updated;
                    send(&tx, Work::Event(json!({"type":"ok","command":"configure"})))?;
                }
                "audio" => session
                    .as_mut()
                    .ok_or("send start first")?
                    .audio(frame, &tx, float_pcm)?,
                "finish" => {
                    fields(&frame.header, &["type", "payload_bytes"])?;
                    let current = session.as_ref().ok_or("send start first")?;
                    if current.opened_settings.is_some() {
                        send(
                            &tx,
                            Work::Event(done(current.next, "skipped", 0, Some("incomplete tail"))),
                        )?;
                    }
                    send(&tx, Work::Finish)?;
                    return Ok(true);
                }
                _ => return Err(format!("unknown command {command}")),
            }
            Ok(false)
        })();
        match result {
            Ok(true) => return, // Finish drains queued work: do not cancel it.
            Ok(false) => {}
            Err(reason) => {
                if send(&tx, Work::Event(error(&command, &reason))).is_err() {
                    break;
                }
            }
        }
    }
    if let Some(session) = session {
        session.cancel_all();
    }
}
