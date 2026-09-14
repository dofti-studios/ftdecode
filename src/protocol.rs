// SPDX-License-Identifier: GPL-3.0-or-later
//! Version-one framing. Lengths are checked before payload allocation.
use serde_json::Value;
use std::io::{self, Read, Write};

pub const MAX_HEADER_BYTES: usize = 8192;
pub const MAX_PCM_BYTES: usize = 24000;

#[derive(Debug)]
pub struct Frame {
    pub header: Value,
    pub payload: Vec<u8>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut length = [0; 4];
    loop {
        match reader.read(&mut length[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    reader.read_exact(&mut length[1..])?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_HEADER_BYTES {
        return Err(invalid("JSON header length must be 1..8192 bytes"));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    let header: Value = serde_json::from_slice(&bytes).map_err(|e| invalid(&e.to_string()))?;
    let kind = header
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("header requires a string type"))?;
    let count = header
        .get("payload_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("payload_bytes must be a nonnegative integer"))?;
    if count > MAX_PCM_BYTES as u64 || (kind != "audio" && count != 0) {
        return Err(invalid(
            "only audio may carry payload, limited to 24000 bytes",
        ));
    }
    if kind == "audio" && (count == 0 || count % 2 != 0) {
        return Err(invalid(
            "audio payload must contain 1..12000 complete s16le samples",
        ));
    }
    let mut payload = vec![0; count as usize];
    reader.read_exact(&mut payload)?;
    Ok(Some(Frame { header, payload }))
}

pub fn write_frame<W: Write>(writer: &mut W, header: &Value) -> io::Result<()> {
    let mut header = header.clone();
    header
        .as_object_mut()
        .ok_or_else(|| invalid("output header must be an object"))?
        .insert("payload_bytes".into(), Value::from(0));
    let bytes = serde_json::to_vec(&header).map_err(|e| invalid(&e.to_string()))?;
    if bytes.len() > MAX_HEADER_BYTES {
        return Err(invalid("output header exceeds 8192 bytes"));
    }
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}
