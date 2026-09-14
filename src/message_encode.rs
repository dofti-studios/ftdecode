// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded 77-bit payload synthesis for receive assistance.
//!
//! Wire layouts and call conventions follow WSJT-X `packjt77.f90` at
//! ccdfaf3c1c109010d15399674ce278167cfde848. See NOTICE.md.
//! This API rejects unsupported input instead of silently falling back to
//! truncated free text. It has no callsign-history side effects.
use crate::message::{MessageError, tables};
const ALPHABET: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";
const TOKENS: u64 = 2_063_592;

/// Encode standard, DXpedition, Field Day, RTTY, nonstandard-call and VHF
/// exchanges. ASCII case and repeated spaces are normalized (input is limited
/// to 37 ASCII characters before normalization). Hashed and nonstandard calls
/// must fit the native 11-character field.
///
/// Standard signed dB reports accept -50..=50. DXpedition reports accept
/// -30..=32 in 2 dB steps: odd inputs round down, e.g. -11 becomes -12 and
/// +11 becomes +10, matching native packing. RTTY reports are 529, 539, ...,
/// 599; VHF reports are 52..=59 with serials 1..=2047.
///
/// Standard compound-call messages use the native base-call convention; `/P`
/// and `/R` are retained. Other prefixes/suffixes need the explicit type-4 form
/// (e.g. `<W9XYZ> PJ4/K1ABC RR73`) to retain the complete compound call.
/// Free text, telemetry, WSPR, ambiguous unbracketed type-4 pairs, and exchanges
/// requiring native field truncation or range clamping are deliberately unsupported.
/// DXpedition report quantization and the base-call convention above are retained.
pub fn encode(text: &str) -> Result<[bool; 77], MessageError> {
    if text.len() > 37 || !text.bytes().all(|b| b.is_ascii_graphic() || b == b' ') {
        return Err(MessageError::InvalidField);
    }
    let normalized = text.to_ascii_uppercase();
    let mut words: Vec<String> = normalized.split_whitespace().map(str::to_owned).collect();
    if words.iter().any(|w| w.len() > 13) || words.len() < 2 {
        return Err(MessageError::UnsupportedType);
    }
    if words.len() >= 3 && words[0] == "CQ" && base_call(&words[2]).is_some() {
        let cq = format!("CQ_{}", words[1]);
        token(&cq).ok_or(MessageError::InvalidField)?;
        words[0] = cq;
        words.remove(1);
    }
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    if w.len() == 5 && w[1] == "RR73;" {
        base_call(w[0]).ok_or(MessageError::InvalidCallsign)?;
        base_call(w[2]).ok_or(MessageError::InvalidCallsign)?;
        let report = signed_report(w[4])?;
        if !(-30..=32).contains(&report) {
            return Err(MessageError::InvalidField);
        }
        return Ok(pack(&[
            (call28(w[0])?, 28),
            (call28(w[2])?, 28),
            (hash(bracket_call(w[3])?, 10), 10),
            (((report + 30) / 2) as u64, 5),
            (1, 3),
            (0, 3),
        ]));
    }
    if let Some(bits) = field_day(&w)? {
        return Ok(bits);
    }
    if let Some(bits) = standard(&w)? {
        return Ok(bits);
    }
    if let Some(bits) = rtty(&w)? {
        return Ok(bits);
    }
    if let Some(bits) = nonstandard(&w)? {
        return Ok(bits);
    }
    if let Some(bits) = vhf(&w)? {
        return Ok(bits);
    }
    Err(MessageError::UnsupportedType)
}

fn pack(fields: &[(u64, usize)]) -> [bool; 77] {
    let mut bits = [false; 77];
    let mut offset = 0;
    for &(value, width) in fields {
        debug_assert!(value < (1_u64 << width));
        for j in 0..width {
            bits[offset + j] = value & (1 << (width - 1 - j)) != 0;
        }
        offset += width;
    }
    debug_assert_eq!(offset, 77);
    bits
}

fn valid_call(call: &str) -> bool {
    (3..=11).contains(&call.len())
        && call
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'/')
        && call.bytes().any(|b| b.is_ascii_uppercase())
        && call.bytes().any(|b| b.is_ascii_digit())
        && !call.starts_with('/')
        && !call.ends_with('/')
        && call.bytes().filter(|&b| b == b'/').count() <= 1
}

fn bracket_call(call: &str) -> Result<&str, MessageError> {
    call.strip_prefix('<')
        .and_then(|c| c.strip_suffix('>'))
        .filter(|c| valid_call(c))
        .ok_or(MessageError::InvalidCallsign)
}

// Native chkcall chooses the longer side of a compound call (the right on ties).
fn base_call(call: &str) -> Option<&str> {
    if !valid_call(call) {
        return None;
    }
    let base = if let Some((a, b)) = call.split_once('/') {
        if a.len().max(b.len()) > 6 {
            return None;
        }
        if a.len() > b.len() { a } else { b }
    } else {
        call
    };
    let b = base.as_bytes();
    if !(3..=6).contains(&b.len())
        || (b[0] == b'Q' && base != "QU1RK")
        || !b[..2].iter().any(u8::is_ascii_uppercase)
    {
        return None;
    }
    let area = if b[2].is_ascii_digit() {
        2
    } else if b[1].is_ascii_digit() {
        1
    } else {
        return None;
    };
    if !(1..=3).contains(&(b.len() - area - 1)) || !b[area + 1..].iter().all(u8::is_ascii_uppercase)
    {
        return None;
    }
    Some(base)
}

fn hash(call: &str, width: u32) -> u64 {
    let n = call
        .bytes()
        .chain(std::iter::repeat(b' '))
        .take(11)
        .fold(0_u64, |n, b| {
            n * 38 + ALPHABET.iter().position(|&a| a == b).unwrap() as u64
        });
    n.wrapping_mul(47_055_833_459) >> (64 - width)
}

fn token(call: &str) -> Option<u64> {
    match call {
        "DE" => return Some(0),
        "QRZ" => return Some(1),
        "CQ" => return Some(2),
        _ => (),
    }
    let s = call.strip_prefix("CQ_")?;
    if s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()) {
        return Some(3 + s.parse::<u64>().ok()?);
    }
    if (1..=4).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_uppercase()) {
        return Some(1003 + s.bytes().fold(0, |n, b| n * 27 + u64::from(b - b'A' + 1)));
    }
    None
}

fn call28(call: &str) -> Result<u64, MessageError> {
    if let Some(n) = token(call) {
        return Ok(n);
    }
    if call.starts_with('<') {
        return Ok(TOKENS + hash(bracket_call(call)?, 22));
    }
    if !valid_call(call) {
        return Err(MessageError::InvalidCallsign);
    }
    if base_call(call) != Some(call) {
        return Ok(TOKENS + hash(call, 22));
    }
    let area = call.bytes().rposition(|b| b.is_ascii_digit()).unwrap();
    let mut padded = [b' '; 6];
    let start = usize::from(area == 1);
    padded[start..start + call.len()].copy_from_slice(call.as_bytes());
    let alphabets: [&[u8]; 6] = [
        b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        b"0123456789",
        b" ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        b" ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        b" ABCDEFGHIJKLMNOPQRSTUVWXYZ",
    ];
    let mut n = 0;
    for (b, alphabet) in padded.into_iter().zip(alphabets) {
        n = n * alphabet.len() as u64
            + alphabet
                .iter()
                .position(|&a| a == b)
                .ok_or(MessageError::InvalidCallsign)? as u64;
    }
    Ok(n + TOKENS + (1 << 22))
}

fn grid4(grid: &str) -> Option<u64> {
    let b = grid.as_bytes();
    if b.len() != 4
        || !(b'A'..=b'R').contains(&b[0])
        || !(b'A'..=b'R').contains(&b[1])
        || !b[2..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    Some(
        u64::from(b[0] - b'A') * 1800
            + u64::from(b[1] - b'A') * 100
            + u64::from(b[2] - b'0') * 10
            + u64::from(b[3] - b'0'),
    )
}
fn signed_report(s: &str) -> Result<i32, MessageError> {
    if !(2..=3).contains(&s.len())
        || !matches!(s.as_bytes()[0], b'+' | b'-')
        || !s.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(MessageError::InvalidField);
    }
    s.parse().map_err(|_| MessageError::InvalidField)
}

fn standard(w: &[&str]) -> Result<Option<[bool; 77]>, MessageError> {
    if !(2..=4).contains(&w.len()) {
        return Ok(None);
    }
    let a = if token(w[0]).is_some() || w[0].starts_with('<') {
        Some(w[0])
    } else {
        base_call(w[0])
    };
    let b = if w[1].starts_with('<') {
        Some(w[1])
    } else {
        base_call(w[1])
    };
    let (Some(a), Some(b)) = (a, b) else {
        return Ok(None);
    };
    if (w[0].starts_with('<') && w[1].contains('/'))
        || (w[1].starts_with('<') && w[0].contains('/'))
        || (w.len() == 2 && w[1].contains('/'))
    {
        return Ok(None);
    }
    if w.len() == 4 && w[2] != "R" {
        return Ok(None);
    }
    let last = w[w.len() - 1];
    let (ir, exchange) = if w.len() == 2 {
        (0, 32401)
    } else if let Some(grid) = grid4(last) {
        (u64::from(w.len() == 4), grid)
    } else if w.len() == 3 && matches!(last, "RRR" | "73") {
        (0, if last == "RRR" { 32402 } else { 32404 })
    } else if w.len() == 3
        && (last.starts_with(['+', '-']) || last.starts_with("R+") || last.starts_with("R-"))
    {
        let (ir, s) = if let Some(s) = last.strip_prefix('R') {
            (1, s)
        } else {
            (0, last)
        };
        let mut report = signed_report(s)?;
        if !(-50..=50).contains(&report) {
            return Err(MessageError::InvalidField);
        }
        if report <= -31 {
            report += 101;
        }
        (ir, (32400 + report + 35) as u64)
    } else {
        return Ok(None);
    };
    if w[0].starts_with("CQ")
        && (ir != 0 || exchange > 32401 || last == "RR73" || w[1].starts_with('<'))
    {
        return Err(MessageError::InvalidField);
    }
    let portable = w[..2].iter().any(|c| c.ends_with("/P"));
    if portable && w[..2].iter().any(|c| c.ends_with("/R")) {
        return Err(MessageError::InvalidField);
    }
    let suffix = |s: &str| u64::from(s.ends_with("/P") || s.ends_with("/R"));
    Ok(Some(pack(&[
        (call28(a)?, 28),
        (suffix(w[0]), 1),
        (call28(b)?, 28),
        (suffix(w[1]), 1),
        (ir, 1),
        (exchange, 15),
        (if portable { 2 } else { 1 }, 3),
    ])))
}

fn field_day(w: &[&str]) -> Result<Option<[bool; 77]>, MessageError> {
    if !(4..=5).contains(&w.len()) || base_call(w[0]).is_none() || base_call(w[1]).is_none() {
        return Ok(None);
    }
    let Some(section) = tables::CSEC.iter().position(|&s| s == w[w.len() - 1]) else {
        return Ok(None);
    };
    if w.len() == 5 && w[2] != "R" {
        return Err(MessageError::InvalidField);
    }
    let class = w[w.len() - 2];
    let Some((&letter, count)) = class.as_bytes().split_last() else {
        return Err(MessageError::InvalidField);
    };
    let count = std::str::from_utf8(count)
        .unwrap()
        .parse::<u64>()
        .map_err(|_| MessageError::InvalidField)?;
    if !(1..=32).contains(&count) || !(b'A'..=b'H').contains(&letter) {
        return Err(MessageError::InvalidField);
    }
    Ok(Some(pack(&[
        (call28(w[0])?, 28),
        (call28(w[1])?, 28),
        (u64::from(w.len() == 5), 1),
        ((count - 1) % 16, 4),
        (u64::from(letter - b'A'), 3),
        ((section + 1) as u64, 7),
        (if count <= 16 { 3 } else { 4 }, 3),
        (0, 3),
    ])))
}

fn rtty(w: &[&str]) -> Result<Option<[bool; 77]>, MessageError> {
    let tu = w[0] == "TU;";
    let w = if tu { &w[1..] } else { w };
    if !(4..=5).contains(&w.len()) || base_call(w[0]).is_none() || base_call(w[1]).is_none() {
        return Ok(None);
    }
    if w.len() == 5 && w[2] != "R" {
        return Ok(None);
    }
    let report = w[w.len() - 2].as_bytes();
    if report.len() != 3
        || report[0] != b'5'
        || report[2] != b'9'
        || !(b'2'..=b'9').contains(&report[1])
    {
        return Ok(None);
    }
    let exchange = if let Some(n) = tables::CMULT.iter().position(|&s| s == w[w.len() - 1]) {
        8001 + n as u64
    } else {
        let n = w[w.len() - 1]
            .parse::<u64>()
            .map_err(|_| MessageError::InvalidField)?;
        if !(1..=7999).contains(&n) {
            return Err(MessageError::InvalidField);
        }
        n
    };
    Ok(Some(pack(&[
        (u64::from(tu), 1),
        (call28(w[0])?, 28),
        (call28(w[1])?, 28),
        (u64::from(w.len() == 5), 1),
        (u64::from(report[1] - b'2'), 3),
        (exchange, 13),
        (3, 3),
    ])))
}

fn nonstandard(w: &[&str]) -> Result<Option<[bool; 77]>, MessageError> {
    if !(2..=3).contains(&w.len()) {
        return Ok(None);
    }
    let cq = w[0] == "CQ";
    if w.len() == 3 && (cq || !matches!(w[2], "RRR" | "RR73" | "73")) {
        return Ok(None);
    }
    let (hashed, call, flip) = if cq {
        (w[1], w[1], 0)
    } else if w[0].starts_with('<') {
        (bracket_call(w[0])?, w[1], 0)
    } else if w[1].starts_with('<') {
        (bracket_call(w[1])?, w[0], 1)
    } else {
        return Ok(None);
    };
    if !valid_call(call) || (cq && call.len() <= 4) {
        return Err(MessageError::InvalidCallsign);
    }
    // Two ordinary calls belong to type 1 even when one is bracketed.
    if base_call(call) == Some(call) && base_call(hashed) == Some(hashed) && !cq {
        return Ok(None);
    }
    let n = call.bytes().fold(0_u64, |n, b| {
        n * 38 + ALPHABET.iter().position(|&a| a == b).unwrap() as u64
    });
    let report = match w.get(2).copied() {
        Some("RRR") => 1,
        Some("RR73") => 2,
        Some("73") => 3,
        _ => 0,
    };
    Ok(Some(pack(&[
        (hash(hashed, 12), 12),
        (n, 58),
        (flip, 1),
        (report, 2),
        (u64::from(cq), 1),
        (4, 3),
    ])))
}

fn vhf(w: &[&str]) -> Result<Option<[bool; 77]>, MessageError> {
    if !(4..=5).contains(&w.len()) || !w[0].starts_with('<') || !w[1].starts_with('<') {
        return Ok(None);
    }
    if w.len() == 5 && w[2] != "R" {
        return Err(MessageError::InvalidField);
    }
    let exchange = w[w.len() - 2];
    if exchange.len() != 6 || !exchange.bytes().all(|b| b.is_ascii_digit()) {
        return Err(MessageError::InvalidField);
    }
    let n = exchange
        .parse::<u64>()
        .map_err(|_| MessageError::InvalidField)?;
    if !(52..=59).contains(&(n / 10000)) || !(1..=2047).contains(&(n % 10000)) {
        return Err(MessageError::InvalidField);
    }
    let grid = w[w.len() - 1];
    if grid.len() != 6
        || !grid.as_bytes()[4..]
            .iter()
            .all(|b| (b'A'..=b'X').contains(b))
    {
        return Err(MessageError::InvalidField);
    }
    let grid = grid4(&grid[..4]).ok_or(MessageError::InvalidField)? * 576
        + u64::from(grid.as_bytes()[4] - b'A') * 24
        + u64::from(grid.as_bytes()[5] - b'A');
    Ok(Some(pack(&[
        (hash(bracket_call(w[0])?, 12), 12),
        (hash(bracket_call(w[1])?, 22), 22),
        (u64::from(w.len() == 5), 1),
        (n / 10000 - 52, 3),
        (n % 10000, 11),
        (grid, 25),
        (5, 3),
    ])))
}
