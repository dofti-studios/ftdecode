// SPDX-License-Identifier: GPL-3.0-or-later
//! Adapters let WAV and framed streaming use the very same session/decoder.
use crate::audio_input::ConvertedWav;
use serde_json::{Value, json};
use std::io::{self, Read, Seek, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn encode(header: Value, payload: &[u8]) -> Vec<u8> {
    let bytes = serde_json::to_vec(&header).expect("generated JSON header");
    let mut frame = Vec::with_capacity(4 + bytes.len() + payload.len());
    frame.extend((bytes.len() as u32).to_le_bytes());
    frame.extend(bytes);
    frame.extend(payload);
    frame
}

pub struct WavFrames<R> {
    wav: ConvertedWav<R>,
    pending: Vec<u8>,
    offset: usize,
    first: u64,
    finished: bool,
}
impl<R: Read + Seek> WavFrames<R> {
    pub fn new(wav: ConvertedWav<R>, start: Value) -> Self {
        Self {
            wav,
            pending: encode(start, &[]),
            offset: 0,
            first: 0,
            finished: false,
        }
    }
}
impl<R: Read + Seek> Read for WavFrames<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.offset == self.pending.len() {
            if self.finished {
                return Ok(0);
            }
            // Float frames remain within the existing bounded frame byte limit.
            let audio = self.wav.read_samples(6000)?;
            if audio.is_empty() {
                self.pending = encode(json!({"type":"finish","payload_bytes":0}), &[]);
                self.finished = true;
            } else {
                let mut bytes = Vec::with_capacity(audio.len() * 4);
                for value in &audio {
                    bytes.extend(value.to_le_bytes());
                }
                self.pending = encode(
                    json!({"type":"audio","payload_bytes":bytes.len(),"first_sample":self.first.to_string(),"sample_count":audio.len()}),
                    &bytes,
                );
                self.first += audio.len() as u64;
            }
            self.offset = 0;
        }
        let n = output.len().min(self.pending.len() - self.offset);
        output[..n].copy_from_slice(&self.pending[self.offset..self.offset + n]);
        self.offset += n;
        Ok(n)
    }
}

/// Converts server frames to JSON lines, without buffering audio or recordings.
pub struct JsonLines<W> {
    writer: W,
    pending: Vec<u8>,
    header_len: Option<usize>,
    errors: Arc<AtomicBool>,
}
impl<W: Write> JsonLines<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            pending: Vec::new(),
            header_len: None,
            errors: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn error_flag(&self) -> Arc<AtomicBool> {
        self.errors.clone()
    }
}
impl<W: Write> Write for JsonLines<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let target = self.header_len.unwrap_or(4);
        let count = (target - self.pending.len()).min(bytes.len());
        self.pending.extend_from_slice(&bytes[..count]);
        if self.pending.len() == target {
            if self.header_len.is_none() {
                let n = u32::from_le_bytes(self.pending[..4].try_into().unwrap()) as usize;
                if n == 0 || n > 8192 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid output header length",
                    ));
                }
                self.pending.clear();
                self.header_len = Some(n);
            } else {
                let v: Value = serde_json::from_slice(&self.pending)?;
                if v["payload_bytes"] != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unexpected binary result",
                    ));
                }
                if v["type"] == "error" || (v["type"] == "done" && v["status"] == "failed") {
                    self.errors.store(true, Ordering::Relaxed);
                }
                self.writer.write_all(&self.pending)?;
                self.writer.write_all(b"\n")?;
                self.pending.clear();
                self.header_len = None;
            }
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Map source-frame timing to the first decoder sample at or after the boundary.
pub fn source_offset(frames: u64, rate: u32) -> Result<u64, String> {
    if rate == 0 {
        return Err("source sample rate must be positive".into());
    }
    u64::try_from((u128::from(frames) * 12000).div_ceil(u128::from(rate)))
        .map_err(|_| "slot offset exceeds decoder sample range".into())
}

/// Convert nonnegative decimal seconds to a count of 12 kHz samples.
/// Accepts up to nine fractional digits, using exact integer arithmetic and
/// rounding upward to the first sample at or after the requested boundary.
/// The returned value is samples, not nanoseconds.
///
/// ```
/// assert_eq!(ftdecode::wav_cli::seconds_offset("1")?, 12_000);
/// assert_eq!(ftdecode::wav_cli::seconds_offset("0.000000001")?, 1);
/// # Ok::<(), String>(())
/// ```
pub fn seconds_offset(value: &str) -> Result<u64, String> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 9
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (value.contains('.') && fraction.is_empty())
    {
        return Err("slot offset seconds must be nonnegative decimal seconds with at most 9 fractional digits".into());
    }
    let whole: u64 = whole
        .parse()
        .map_err(|_| "slot offset seconds exceeds range")?;
    let fraction: u64 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().unwrap()
    };
    let places = value.split_once('.').map_or(0, |(_, f)| f.len());
    let nanos =
        u128::from(whole) * 1_000_000_000 + u128::from(fraction) * 10u128.pow(9 - places as u32);
    u64::try_from((nanos * 12000).div_ceil(1_000_000_000))
        .map_err(|_| "slot offset exceeds decoder sample range".into())
}
