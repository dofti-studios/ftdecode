// SPDX-License-Identifier: GPL-3.0-or-later
//! Explicit receive assistance context; no radio control or contact sequencer.
use crate::engine::Mode;

macro_rules! named_enum {
    ($name:ident, $default:ident, $( $variant:ident => $text:literal ),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name { $($variant),+ }
        impl Default for $name { fn default() -> Self { Self::$default } }
        impl $name { pub fn name(self) -> &'static str { match self { $(Self::$variant => $text),+ } } }
        impl std::str::FromStr for $name {
            type Err = String;
            fn from_str(value: &str) -> Result<Self, String> {
                match value { $($text => Ok(Self::$variant)),+, _ => Err(format!("invalid {}: {value}", stringify!($name))) }
            }
        }
    }
}
named_enum!(ApMode, Off, Off => "off", Cq => "cq", Auto => "auto");
named_enum!(ContactState, CallingCq, CallingCq => "calling-cq", CallingStation => "calling-station", Report => "report", RogerReport => "roger-report", Rogers => "rogers", Signoff => "signoff");
named_enum!(Activity, Normal, Normal => "normal", NaVhf => "na-vhf", EuVhf => "eu-vhf", FieldDay => "field-day", Rtty => "rtty", WwDigi => "ww-digi", Fox => "fox", Hound => "hound", ArrlDigi => "arrl-digi");
impl Activity {
    pub fn native(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::NaVhf => 1,
            Self::EuVhf => 2,
            Self::FieldDay => 3,
            Self::Rtty => 4,
            Self::WwDigi => 5,
            Self::Fox => 6,
            Self::Hound => 7,
            Self::ArrlDigi => 8,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApSettings {
    pub mode: ApMode,
    pub my_call: Option<String>,
    pub dx_call: Option<String>,
    pub dx_grid: Option<String>,
    pub state: ContactState,
    pub activity: Activity,
    pub width_hz: f32,
    pub tx_hz: Option<f32>,
    pub known_calls: Vec<String>,
}
impl Default for ApSettings {
    fn default() -> Self {
        Self {
            mode: ApMode::Off,
            my_call: None,
            dx_call: None,
            dx_grid: None,
            state: ContactState::CallingCq,
            activity: Activity::Normal,
            width_hz: 50.0,
            tx_hz: None,
            known_calls: Vec::new(),
        }
    }
}
impl ApSettings {
    pub fn validate(&self) -> Result<(), String> {
        for (call, max_len) in self
            .my_call
            .iter()
            .chain(self.dx_call.iter())
            .map(|c| (c, 11))
            .chain(self.known_calls.iter().map(|c| (c, 13)))
        {
            let trimmed = call.trim();
            let normalized = if trimmed.starts_with('<') && trimmed.ends_with('>') {
                &trimmed[1..trimmed.len() - 1]
            } else {
                trimmed
            };
            if normalized.len() < 3
                || normalized.len() > max_len
                || !normalized
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'/')
                || !normalized.bytes().any(|b| b.is_ascii_alphabetic())
                || !normalized.bytes().any(|b| b.is_ascii_digit())
                || normalized.starts_with('/')
                || normalized.ends_with('/')
                || normalized.bytes().filter(|b| *b == b'/').count() > 1
            {
                return Err(format!("invalid assistance callsign: {call}"));
            }
        }
        if self.known_calls.len() > 400 {
            return Err("known_calls is limited to 400 entries".into());
        }
        if self.dx_grid.as_ref().is_some_and(|grid| {
            let g = grid.as_bytes();
            !(g.len() == 4 || g.len() == 6)
                || !g[..2]
                    .iter()
                    .all(|b| (b'A'..=b'R').contains(&b.to_ascii_uppercase()))
                || !g[2..4].iter().all(u8::is_ascii_digit)
                || (g.len() == 6
                    && !g[4..]
                        .iter()
                        .all(|b| (b'A'..=b'X').contains(&b.to_ascii_uppercase())))
        }) {
            return Err("dx_grid must be a four- or six-character Maidenhead locator".into());
        }
        if !self.width_hz.is_finite() || !(0.0..=1000.0).contains(&self.width_hz) {
            return Err("ap_width_hz must be within 0..1000 Hz".into());
        }
        if self
            .tx_hz
            .is_some_and(|f| !f.is_finite() || !(0.0..=5000.0).contains(&f))
        {
            return Err("tx_hz must be within 0..5000 Hz".into());
        }
        Ok(())
    }
    pub fn targeted(&self, frequency: f32, rx: Option<f32>) -> bool {
        rx.into_iter()
            .chain(self.tx_hz)
            .any(|target| (target - frequency).abs() <= self.width_hz)
    }
    /// Native per-contact-state AP schedule. CQ-only bypasses station hints.
    pub fn types(&self, mode: Mode) -> &'static [u8] {
        if self.mode == ApMode::Off {
            return &[];
        }
        if self.mode == ApMode::Cq {
            return &[1];
        }
        match self.state {
            ContactState::CallingCq => &[1, 2],
            ContactState::CallingStation | ContactState::Report => &[2, 3],
            ContactState::RogerReport | ContactState::Rogers if mode == Mode::Ft8 => &[3, 4, 5, 6],
            ContactState::RogerReport | ContactState::Rogers => &[3, 6],
            ContactState::Signoff => &[3, 1, 2],
        }
    }
}

/// A native AP hypothesis. `bits` are in channel order (scrambled for FT4).
#[derive(Clone, Debug)]
pub struct ApHypothesis {
    pub ap_type: u8,
    pub mask: [bool; 174],
    pub bits: [bool; 77],
    // Native FT8 Field Day type 3 freezes n3 without replacing its likelihood.
    preserve: [bool; 77],
}
impl ApHypothesis {
    pub fn apply(&self, llr: &[f32; 174], magnitude: f32) -> [f32; 174] {
        std::array::from_fn(|i| {
            if i < 77 && self.mask[i] && !self.preserve[i] {
                if self.bits[i] { magnitude } else { -magnitude }
            } else {
                llr[i]
            }
        })
    }
    fn set(&mut self, offset: usize, bits: &[bool]) {
        self.mask[offset..offset + bits.len()].fill(true);
        self.bits[offset..offset + bits.len()].copy_from_slice(bits);
    }
}

/// Build the bounded native state schedule, with explicit receive context gates.
/// Known calls seed message hash resolution elsewhere; they do not add AP trials.
pub fn hypotheses(
    settings: &crate::engine::DecodeSettings,
    mode: Mode,
    frequency: f32,
) -> Vec<ApHypothesis> {
    if settings.ap_mode() == ApMode::Off || (mode == Mode::Ft4 && settings.depth < 2) {
        return Vec::new();
    }
    let ap = &settings.ap;
    let activity = ap.activity.native();
    if activity == 6 || (mode == Mode::Ft4 && activity >= 6) || (activity == 7 && frequency > 950.0)
    {
        return Vec::new();
    }
    let context = call_context(ap, mode);
    let cq = crate::message_encode::encode(match ap.activity {
        Activity::Normal | Activity::Hound => "CQ K1ABC FN42",
        Activity::NaVhf | Activity::EuVhf | Activity::ArrlDigi => "CQ TEST K1ABC FN42",
        Activity::FieldDay => "CQ FD K1ABC FN42",
        Activity::Rtty => "CQ RU K1ABC FN42",
        Activity::WwDigi => "CQ WW K1ABC FN42",
        Activity::Fox => unreachable!(),
    })
    .expect("fixed native CQ token");
    let types = if settings.ap_mode() == ApMode::Cq {
        &[1][..]
    } else {
        ap.types(mode)
    };
    let mut out = Vec::with_capacity(types.len());
    for &ap_type in types {
        if ap_type >= 3
            && activity != 7
            && !(if mode == Mode::Ft4 {
                settings
                    .priority_hz
                    .is_some_and(|rx| (frequency - rx).abs() <= ap.width_hz)
            } else {
                ap.targeted(frequency, settings.priority_hz)
            })
        {
            continue;
        }
        if ap_type >= 2 && context.is_none() {
            continue;
        }
        if ap_type >= 3 && ap.dx_call.is_none() {
            continue;
        }
        if activity == 7 && ap_type >= 2 && ap.dx_call.is_none() {
            continue;
        }
        if activity == 7 && ap_type == 5 {
            continue;
        }
        let calls = context.as_ref().map(|c| &c.0).unwrap_or(&cq);
        let mut h = ApHypothesis {
            ap_type,
            mask: [false; 174],
            bits: [false; 77],
            preserve: [false; 77],
        };
        match ap_type {
            1 => {
                h.set(0, &cq[..29]);
                if mode == Mode::Ft8 {
                    h.set(74, &[false, false, true]);
                }
            }
            2 => match activity {
                0 | 1 | 5 | 8 => {
                    h.set(0, &calls[..29]);
                    if mode == Mode::Ft8 {
                        h.set(74, &[false, false, true]);
                    }
                }
                2 | 3 => {
                    h.set(0, &calls[..28]);
                    if mode == Mode::Ft8 {
                        h.set(74, &[false; 3]);
                        if activity == 2 {
                            h.set(71, &[false, true, false]);
                        }
                    }
                }
                4 => {
                    h.set(1, &calls[..28]);
                    if mode == Mode::Ft8 {
                        h.set(74, &[false, true, true]);
                    }
                }
                7 => {
                    h.set(28, &calls[..28]);
                    let hash = context.as_ref().unwrap().1;
                    h.set(
                        56,
                        &std::array::from_fn::<_, 10, _>(|i| hash & (1 << (9 - i)) != 0),
                    );
                    h.set(71, &[false, false, true, false, false, false]);
                }
                _ => unreachable!(),
            },
            3 => match activity {
                3 => {
                    h.set(0, &calls[..28]);
                    h.set(28, &calls[29..57]);
                    if mode == Mode::Ft8 {
                        h.mask[71..74].fill(true);
                        h.preserve[71..74].fill(true);
                        h.set(74, &[false; 3]);
                    }
                }
                4 => {
                    h.set(1, &calls[..28]);
                    h.set(29, &calls[29..57]);
                    if mode == Mode::Ft8 {
                        h.set(74, &[false, true, true]);
                    }
                }
                _ => {
                    h.set(0, &calls[..58]);
                    if mode == Mode::Ft8 {
                        h.set(74, &[false, false, true]);
                    }
                }
            },
            4 if activity == 7 => {
                h.set(0, &calls[..28]);
                let hash = context.as_ref().unwrap().1;
                h.set(
                    56,
                    &std::array::from_fn::<_, 10, _>(|i| hash & (1 << (9 - i)) != 0),
                );
                h.set(71, &[false, false, true, false, false, false]);
            }
            4..=6 => {
                h.set(0, calls);
                if mode == Mode::Ft8 {
                    let end = match ap_type {
                        4 => "0111111010010010001",
                        5 => "0111111010010100001",
                        _ => "0111111001110101001",
                    };
                    h.set(58, &end.bytes().map(|b| b == b'1').collect::<Vec<_>>());
                }
            }
            _ => unreachable!(),
        }
        if mode == Mode::Ft4 {
            h.bits = crate::symbols::scramble_ft4(&h.bits);
        }
        out.push(h);
    }
    out
}

fn call_context(ap: &ApSettings, mode: Mode) -> Option<([bool; 77], u32)> {
    // Supplied calls name stations; brackets are display syntax for hashes.
    // Strip them before deciding which native payload representation to use.
    let normalize = |call: &str| call.trim().trim_matches(['<', '>']).to_ascii_uppercase();
    let my = normalize(ap.my_call.as_ref()?);
    let dx =
        normalize(
            ap.dx_call
                .as_deref()
                .unwrap_or(if mode == Mode::Ft8 { "KA1ABC" } else { &my }),
        );
    let mut decoder = crate::message::MessageDecoder::default();
    decoder.remember_call(&my).ok()?;
    let hash = decoder.remember_call(&dx).ok()?[0];
    let first = if mode == Mode::Ft8 && !native_stdcall(&my) {
        format!("<{my}>")
    } else {
        my
    };
    let text = format!(
        "{first} {dx} {}",
        if mode == Mode::Ft8 { "RRR" } else { "RR73" }
    );
    let bits = crate::message_encode::encode(&text).ok().or_else(|| {
        // Native ft8apset omits the roundtrip/type gate for ARRL digi. Its
        // internal dummy pack can therefore fall back to 13-character text.
        // This is scoped to native AP setup, not the public message encoder.
        if ap.activity != Activity::ArrlDigi {
            return None;
        }
        let alphabet = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-./?";
        let truncated = &text.as_bytes()[..text.len().min(13)];
        let value = truncated
            .iter()
            .rev()
            .skip_while(|b| **b == b' ')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .fold(0_u128, |n, b| {
                n * 42 + alphabet.iter().position(|c| c == b).unwrap_or(0) as u128
            });
        Some(std::array::from_fn(|i| {
            i < 71 && value & (1 << (70 - i)) != 0
        }))
    })?;
    if mode == Mode::Ft4 || ap.activity.native() <= 5 || ap.activity == Activity::Hound {
        if bits[74..] != [false, false, true] {
            return None;
        }
        let unpacked = decoder.unpack(&bits).ok()?;
        if ap.activity != Activity::Hound && unpacked.text != text {
            return None;
        }
    }
    Some((bits, hash))
}

// WSJT-X q65_set_list.f90 stdcall: this deliberately differs from wire packing.
fn native_stdcall(call: &str) -> bool {
    let b = call.as_bytes();
    let Some(area) = b.iter().rposition(u8::is_ascii_digit) else {
        return false;
    };
    (1..=2).contains(&area)
        && b[..area].iter().any(u8::is_ascii_uppercase)
        && b[area + 1..]
            .iter()
            .filter(|b| b.is_ascii_uppercase())
            .count()
            <= 3
}
