// SPDX-License-Identifier: GPL-3.0-or-later
//! Receive-side 77-bit message handling for FT8 and FT4.
//! Ported from WSJT-X `lib/77bit/packjt77.f90` at
//! ccdfaf3c1c109010d15399674ce278167cfde848. See NOTICE.md.
//! Input is the recovered source message (after FT4 descrambling).

use std::collections::VecDeque;
#[path = "message_tables.rs"]
pub(crate) mod tables;
const TOKEN_COUNT: u32 = 2_063_592;
const HASH22_COUNT: u32 = 1 << 22;
const CALL_ALPHABET: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageKind {
    FreeText,
    Standard,
    Dxpedition,
    FieldDay,
    Telemetry,
    Rtty,
    Nonstandard,
    Vhf,
    Wspr,
}
impl MessageKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::FreeText => "free_text",
            Self::Standard => "standard",
            Self::Dxpedition => "dxpedition",
            Self::FieldDay => "field_day",
            Self::Telemetry => "telemetry",
            Self::Rtty => "rtty",
            Self::Nonstandard => "nonstandard",
            Self::Vhf => "vhf",
            Self::Wspr => "wspr",
        }
    }
}

/// Content of a standard QSO exchange, not the receiver's contact state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeType {
    Cq,
    Qrz,
    De,
    Calls,
    Grid,
    RogerGrid,
    Report,
    RogerReport,
    Rrr,
    Rr73,
    Signoff,
}
impl ExchangeType {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cq => "cq",
            Self::Qrz => "qrz",
            Self::De => "de",
            Self::Calls => "calls",
            Self::Grid => "grid",
            Self::RogerGrid => "roger_grid",
            Self::Report => "report",
            Self::RogerReport => "roger_report",
            Self::Rrr => "rrr",
            Self::Rr73 => "rr73",
            Self::Signoff => "73",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallsignHash {
    pub width: u8,
    pub value: u32,
    pub resolved: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Station {
    /// Normalized callsign without display brackets; absent for unresolved hashes.
    pub call: Option<String>,
    pub hash: Option<CallsignHash>,
}
impl Station {
    /// Called with an unpacked callsign field and its hash, never message text.
    fn from_field(call: &str, hash: Option<&CallsignHash>) -> Option<Self> {
        if call.is_empty()
            || matches!(call, "CQ" | "QRZ" | "DE")
            || call.starts_with("CQ ")
            || call.starts_with("CQ_")
        {
            return None;
        }
        if call.starts_with('<') {
            let hash = hash
                .expect("unpacked hashed callsign has a numeric identity")
                .clone();
            Some(Self {
                call: hash.resolved.clone(),
                hash: Some(hash),
            })
        } else {
            Some(Self {
                call: Some(call.to_owned()),
                hash: None,
            })
        }
    }
}

/// Fields recovered during unpacking. Unclassified formats retain null exchange
/// metadata rather than deriving sequencing instructions from displayed text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageFields {
    pub exchange_type: Option<ExchangeType>,
    pub sender: Option<Station>,
    pub recipient: Option<Station>,
    pub cq_modifier: Option<String>,
    /// Signal report carried in a standard message, not measured receive SNR.
    pub report_db: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub text: String,
    pub kind: MessageKind,
    pub grid: Option<String>,
    pub hashes: Vec<CallsignHash>,
    pub fields: MessageFields,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageError {
    UnsupportedType,
    InvalidField,
    InvalidCallsign,
}

/// Receive-local callsign history. Ten/twelve-bit slots use the latest call;
/// the 22-bit history retains at most 1,000 distinct hashes, as in WSJT-X.
/// No process-global state or station-specific override is used.
#[derive(Clone)]
pub struct MessageDecoder {
    calls10: Vec<Option<String>>,
    calls12: Vec<Option<String>>,
    calls22: VecDeque<(u32, String)>,
}
impl Default for MessageDecoder {
    fn default() -> Self {
        Self {
            calls10: vec![None; 1024],
            calls12: vec![None; 4096],
            calls22: VecDeque::new(),
        }
    }
}
impl std::fmt::Display for MessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnsupportedType => "unsupported message type",
            Self::InvalidField => "invalid encoded message field",
            Self::InvalidCallsign => "invalid callsign",
        })
    }
}
impl std::error::Error for MessageError {}

impl MessageDecoder {
    /// Unpack one source message. Only a fully valid message can teach history.
    pub fn unpack(&mut self, bits: &[bool; 77]) -> Result<Message, MessageError> {
        let (mut message, learned) = self.parse(bits)?;
        if message.text.starts_with("CQ <") {
            return Err(MessageError::InvalidField);
        }
        // Receive text has the same 37-character envelope as the wire codec.
        message.text.truncate(37);
        for call in learned {
            self.remember_call(&call)?;
        }
        Ok(message)
    }

    /// Seed a known call, returning its 10-, 12- and 22-bit hashes. Calls are
    /// uppercased and may have surrounding angle brackets. The wire hash uses
    /// the first 11 characters padded with spaces, including wrapping multiply.
    pub fn remember_call(&mut self, call: &str) -> Result<[u32; 3], MessageError> {
        let call = normalize_call(call)?;
        let mut packed = 0_u64;
        for byte in call.bytes().chain(std::iter::repeat(b' ')).take(11) {
            packed = packed * 38 + CALL_ALPHABET.iter().position(|&b| b == byte).unwrap() as u64;
        }
        let mixed = packed.wrapping_mul(47_055_833_459);
        let hashes = [
            (mixed >> 54) as u32,
            (mixed >> 52) as u32,
            (mixed >> 42) as u32,
        ];
        self.calls10[hashes[0] as usize] = Some(call.clone());
        self.calls12[hashes[1] as usize] = Some(call.clone());
        if let Some(entry) = self.calls22.iter_mut().find(|e| e.0 == hashes[2]) {
            entry.1 = call;
        } else {
            self.calls22.push_front((hashes[2], call));
            self.calls22.truncate(1000);
        }
        Ok(hashes)
    }
    pub fn reset(&mut self) {
        self.calls10.fill(None);
        self.calls12.fill(None);
        self.calls22.clear();
    }

    fn hashed(&self, value: u32, width: u8, hashes: &mut Vec<CallsignHash>) -> String {
        let resolved = match width {
            10 => self.calls10[value as usize].clone(),
            12 => self.calls12[value as usize].clone(),
            22 => self
                .calls22
                .iter()
                .find(|e| e.0 == value)
                .map(|e| e.1.clone()),
            _ => unreachable!("wire hash widths are fixed"),
        };
        let text = format!("<{}>", resolved.as_deref().unwrap_or("..."));
        hashes.push(CallsignHash {
            width,
            value,
            resolved,
        });
        text
    }

    fn call28(&self, value: u32, hashes: &mut Vec<CallsignHash>) -> Result<String, MessageError> {
        let token = match value {
            0 => Some("DE".to_owned()),
            1 => Some("QRZ".to_owned()),
            2 => Some("CQ".to_owned()),
            3..=1002 => Some(format!("CQ_{:03}", value - 3)),
            1003..=532443 => Some(format!(
                "CQ_{}",
                base_text((value - 1003) as u128, 4, b" ABCDEFGHIJKLMNOPQRSTUVWXYZ")?.trim_start()
            )),
            _ => None,
        };
        if let Some(token) = token {
            if token.trim_end().contains(' ') {
                return Err(MessageError::InvalidField);
            }
            return Ok(token.trim_end().to_owned());
        }
        if value < TOKEN_COUNT {
            return Err(MessageError::InvalidField);
        }
        if value < TOKEN_COUNT + HASH22_COUNT {
            return Ok(self.hashed(value - TOKEN_COUNT, 22, hashes));
        }
        let mut n = value - TOKEN_COUNT - HASH22_COUNT;
        let mut call = [b' '; 6];
        for (i, alphabet) in [
            (5, b" ABCDEFGHIJKLMNOPQRSTUVWXYZ".as_slice()),
            (4, b" ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
            (3, b" ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
            (2, b"0123456789"),
            (1, b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
            (0, b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
        ] {
            call[i] = alphabet[(n % alphabet.len() as u32) as usize];
            n /= alphabet.len() as u32;
        }
        let call = String::from_utf8(call.to_vec()).unwrap().trim().to_owned();
        let area = call.bytes().rposition(|c| c.is_ascii_digit());
        let valid = if let Some(area) = area {
            (area == 1 || area == 2)
                && call[..area].bytes().any(|c| c.is_ascii_alphabetic())
                && call[area + 1..].bytes().all(|c| c.is_ascii_alphabetic())
        } else {
            false
        };
        if !valid || call.starts_with('Q') || call.contains(' ') || call.len() < 3 {
            return Err(MessageError::InvalidCallsign);
        }
        Ok(call)
    }

    fn parse(&self, bits: &[bool; 77]) -> Result<(Message, Vec<String>), MessageError> {
        let get = |start, width| field(bits, start, width) as u32;
        let primary = get(74, 3);
        let subtype = get(71, 3);
        let mut hashes = Vec::new();
        let mut learned = Vec::new();
        let mut grid = None;
        let mut fields = MessageFields::default();
        let text;
        let kind;
        match (primary, subtype) {
            (0, 0) => {
                kind = MessageKind::FreeText;
                text = base_text(
                    field(bits, 0, 71),
                    13,
                    b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-./?",
                )?
                .trim()
                .to_owned();
                if text.is_empty() {
                    return Err(MessageError::InvalidField);
                }
            }
            (0, 1) => {
                kind = MessageKind::Dxpedition;
                if get(0, 28) <= 2 || get(28, 28) <= 2 {
                    return Err(MessageError::InvalidField);
                }
                let a = self.call28(get(0, 28), &mut hashes)?;
                let b = self.call28(get(28, 28), &mut hashes)?;
                let c = self.hashed(get(56, 10), 10, &mut hashes);
                text = format!("{a} RR73; {b} {c} {:+03}", 2 * get(66, 5) as i32 - 30);
            }
            (0, 3 | 4) => {
                kind = MessageKind::FieldDay;
                if get(0, 28) <= 2 || get(28, 28) <= 2 {
                    return Err(MessageError::InvalidField);
                }
                let a = self.call28(get(0, 28), &mut hashes)?;
                fields.recipient = Station::from_field(&a, hashes.last());
                let b = self.call28(get(28, 28), &mut hashes)?;
                fields.sender = Station::from_field(&b, hashes.last());
                let count = get(57, 4) + 1 + if subtype == 4 { 16 } else { 0 };
                let class = (b'A' + get(61, 3) as u8) as char;
                let section = get(64, 7)
                    .checked_sub(1)
                    .and_then(|i| tables::CSEC.get(i as usize))
                    .ok_or(MessageError::InvalidField)?;
                text = format!(
                    "{a} {b} {}{count}{class} {section}",
                    if bits[56] { "R " } else { "" }
                );
            }
            (0, 5) => {
                kind = MessageKind::Telemetry;
                let value = field(bits, 0, 71);
                text = if value == 0 {
                    String::new()
                } else {
                    format!("{value:X}")
                };
            }
            (0, 6) => {
                kind = MessageKind::Wspr;
                if bits[49] {
                    // WSPR type 2, 28-bit call + prefix/suffix + power
                    let base = self.call28(get(0, 28), &mut hashes)?;
                    let p = get(28, 16);
                    let power = (get(44, 5) * 10 + 1) / 3;
                    if power > 60 {
                        return Err(MessageError::InvalidField);
                    }
                    let call = if p < 46656 {
                        let prefix =
                            base_text(p as u128, 3, b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ")?;
                        let prefix = prefix.trim_start_matches('0');
                        format!("{}/{base}", if prefix.is_empty() { "0" } else { prefix })
                    } else {
                        let s = p - 46656;
                        let suffix = match s {
                            0..=35 => {
                                base_text(s as u128, 1, b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ")?
                            }
                            36..=1295 => {
                                base_text(s as u128, 2, b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ")?
                            }
                            1296..=12959 => format!(
                                "{}{}{}",
                                CALL_ALPHABET[(s / 360 + 1) as usize] as char,
                                CALL_ALPHABET[(s / 10 % 36 + 1) as usize] as char,
                                s % 10
                            ),
                            _ => return Err(MessageError::InvalidField),
                        };
                        format!("{base}/{suffix}")
                    };
                    learned.push(call.clone());
                    fields.sender = Station::from_field(&call, hashes.last());
                    text = format!("{call} {power}");
                } else if !bits[48] {
                    // WSPR type 1
                    let call = self.call28(get(0, 28), &mut hashes)?;
                    let g = grid_text(get(28, 15), false)?;
                    let power = (get(43, 5) * 10 + 1) / 3;
                    if power > 60 {
                        return Err(MessageError::InvalidField);
                    }
                    text = format!("{call} {g} {power}");
                    fields.sender = Station::from_field(&call, hashes.last());
                    grid = Some(g);
                    learned.push(call);
                } else if !bits[47] {
                    // WSPR type 3, different 25x25 subsquare convention
                    let call = self.hashed(get(0, 22), 22, &mut hashes);
                    fields.sender = Station::from_field(&call, hashes.last());
                    let n = get(22, 25);
                    let g = grid_text(n / 625, false)?;
                    let sub = n % 625;
                    let g = if sub == 624 {
                        g
                    } else {
                        format!(
                            "{g}{}{}",
                            (b'A' + (sub / 25) as u8) as char,
                            (b'A' + (sub % 25) as u8) as char
                        )
                    };
                    text = format!("{call} {g}");
                    grid = Some(g);
                } else {
                    return Err(MessageError::InvalidField);
                }
            }
            (1 | 2, _) => {
                kind = MessageKind::Standard;
                let mut a = self.call28(get(0, 28), &mut hashes)?;
                let a_hash = hashes.last().cloned();
                let mut b = self.call28(get(29, 28), &mut hashes)?;
                if a.starts_with("CQ_") {
                    a.replace_range(2..3, " ");
                    fields.cq_modifier = a.strip_prefix("CQ ").map(str::to_owned);
                }
                for (call, suffix) in [(&mut a, bits[28]), (&mut b, bits[57])] {
                    if !call.starts_with('<') && call.len() >= 3 && suffix {
                        call.push_str(if primary == 1 { "/R" } else { "/P" });
                    }
                }
                // Token values cannot become station identities through a
                // suffix flag, even when the display preserves that flag.
                if get(0, 28) >= TOKEN_COUNT {
                    fields.recipient = Station::from_field(&a, a_hash.as_ref());
                }
                if get(29, 28) >= TOKEN_COUNT {
                    fields.sender = Station::from_field(&b, hashes.last());
                }
                if !b.starts_with('<') && b.len() >= 3 {
                    learned.push(b.clone());
                }
                let encoded = get(59, 15);
                if encoded <= 32400 {
                    let g = grid_text(encoded, false)?;
                    text = format!("{a} {b} {}{g}", if bits[58] { "R " } else { "" });
                    if text.starts_with("CQ ") && bits[58] {
                        return Err(MessageError::InvalidField);
                    }
                    // RR73 can also arrive through the grid-shaped wire value.
                    // Preserve the text without treating it as a location.
                    fields.exchange_type = if g == "RR73" {
                        // "R RR73" is not an ordinary acknowledgement.
                        (!bits[58]).then_some(ExchangeType::Rr73)
                    } else {
                        Some(if bits[58] {
                            ExchangeType::RogerGrid
                        } else {
                            ExchangeType::Grid
                        })
                    };
                    grid = (g != "RR73").then_some(g);
                } else {
                    let report = encoded - 32400;
                    let suffix = match report {
                        1 => {
                            fields.exchange_type = Some(ExchangeType::Calls);
                            String::new()
                        }
                        2 => {
                            fields.exchange_type = Some(ExchangeType::Rrr);
                            " RRR".to_owned()
                        }
                        3 => {
                            fields.exchange_type = Some(ExchangeType::Rr73);
                            " RR73".to_owned()
                        }
                        4 => {
                            fields.exchange_type = Some(ExchangeType::Signoff);
                            " 73".to_owned()
                        }
                        _ => {
                            let mut snr = report as i32 - 35;
                            if snr > 50 {
                                snr -= 101;
                            }
                            fields.report_db = Some(snr);
                            fields.exchange_type = Some(if bits[58] {
                                ExchangeType::RogerReport
                            } else {
                                ExchangeType::Report
                            });
                            let formatted = if snr >= 100 {
                                snr.to_string()
                            } else {
                                format!("{snr:+03}")
                            };
                            format!(" {}{formatted}", if bits[58] { "R" } else { "" })
                        }
                    };
                    text = format!("{a} {b}{suffix}");
                    if text.starts_with("CQ ") && report >= 2 {
                        return Err(MessageError::InvalidField);
                    }
                }
                // Address tokens are encoded fields, not callsigns or contact
                // states. Preserve any carried grid/report independently.
                let address = get(0, 28);
                match address {
                    0 => fields.exchange_type = Some(ExchangeType::De),
                    1 => fields.exchange_type = Some(ExchangeType::Qrz),
                    2..=532443 => {
                        fields.exchange_type = Some(ExchangeType::Cq);
                    }
                    _ => {}
                }
            }
            (3, _) => {
                kind = MessageKind::Rtty;
                let a = self.call28(get(1, 28), &mut hashes)?;
                fields.recipient = Station::from_field(&a, hashes.last());
                let b = self.call28(get(29, 28), &mut hashes)?;
                fields.sender = Station::from_field(&b, hashes.last());
                let exchange = get(61, 13);
                let exchange = match exchange {
                    1..=7999 => format!("{exchange:04}"),
                    8001..=8171 => tables::CMULT[(exchange - 8001) as usize].to_owned(),
                    _ => return Err(MessageError::InvalidField),
                };
                text = format!(
                    "{}{a} {b} {}5{}9 {exchange}",
                    if bits[0] { "TU; " } else { "" },
                    if bits[57] { "R " } else { "" },
                    get(58, 3) + 2
                );
            }
            (4, _) => {
                kind = MessageKind::Nonstandard;
                let hashed = if bits[73] && !bits[70] {
                    String::new() // CQ carries no destination hash.
                } else {
                    self.hashed(get(0, 12), 12, &mut hashes)
                };
                let call = base_text(field(bits, 12, 58), 11, CALL_ALPHABET)?
                    .trim()
                    .to_owned();
                normalize_call(&call)?;
                let (a, b) = if bits[70] {
                    (call, hashed)
                } else {
                    learned.push(call.clone());
                    (hashed, call)
                };
                if bits[73] {
                    text = format!("CQ {b}");
                    fields.exchange_type = Some(ExchangeType::Cq);
                } else {
                    fields.recipient = Station::from_field(&a, hashes.last());
                    fields.exchange_type = Some(
                        [
                            ExchangeType::Calls,
                            ExchangeType::Rrr,
                            ExchangeType::Rr73,
                            ExchangeType::Signoff,
                        ][get(71, 2) as usize],
                    );
                    text = format!(
                        "{a} {b}{}",
                        ["", " RRR", " RR73", " 73"][get(71, 2) as usize]
                    );
                }
                fields.sender = Station::from_field(&b, hashes.last());
            }
            (5, _) => {
                kind = MessageKind::Vhf;
                let a = self.hashed(get(0, 12), 12, &mut hashes);
                fields.recipient = Station::from_field(&a, hashes.last());
                let b = self.hashed(get(12, 22), 22, &mut hashes);
                fields.sender = Station::from_field(&b, hashes.last());
                let g = grid_text(get(49, 25), true)?;
                text = format!(
                    "{a} {b} {}{}{:04} {g}",
                    if bits[34] { "R " } else { "" },
                    52 + get(35, 3),
                    get(38, 11)
                );
                grid = Some(g);
            }
            _ => return Err(MessageError::UnsupportedType),
        }
        // The reference ignores unresolved placeholders when saving calls.
        learned.retain(|call| call != "<...>");
        // Validate all learnable calls before committing any history changes.
        for call in &learned {
            normalize_call(call)?;
        }
        Ok((
            Message {
                text,
                kind,
                grid,
                hashes,
                fields,
            },
            learned,
        ))
    }
}

fn field(bits: &[bool; 77], start: usize, width: usize) -> u128 {
    bits[start..start + width]
        .iter()
        .fold(0_u128, |v, &b| (v << 1) | u128::from(b))
}
fn base_text(mut value: u128, width: usize, alphabet: &[u8]) -> Result<String, MessageError> {
    let mut result = vec![0; width];
    for i in (0..width).rev() {
        result[i] = alphabet[(value % alphabet.len() as u128) as usize];
        value /= alphabet.len() as u128;
    }
    if value != 0 {
        return Err(MessageError::InvalidField);
    }
    Ok(String::from_utf8(result).expect("wire alphabet is ASCII"))
}
fn normalize_call(call: &str) -> Result<String, MessageError> {
    let call = call.trim();
    let call = if call.starts_with('<') && call.ends_with('>') {
        &call[1..call.len() - 1]
    } else {
        call
    };
    let call = call.to_ascii_uppercase();
    if !(3..=13).contains(&call.len())
        || !call
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'/')
    {
        return Err(MessageError::InvalidCallsign);
    }
    Ok(call)
}
fn grid_text(n: u32, six: bool) -> Result<String, MessageError> {
    let grid = if six { n / 576 } else { n };
    if grid >= 32400 {
        return Err(MessageError::InvalidField);
    }
    let mut result = format!(
        "{}{}{}{}",
        (b'A' + (grid / 1800) as u8) as char,
        (b'A' + (grid / 100 % 18) as u8) as char,
        grid / 10 % 10,
        grid % 10
    );
    if six {
        result.push((b'A' + (n / 24 % 24) as u8) as char);
        result.push((b'A' + (n % 24) as u8) as char);
    }
    Ok(result)
}
