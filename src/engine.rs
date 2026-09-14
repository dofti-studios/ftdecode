// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared decoder types. Audio is mono 12 kHz, in raw PCM amplitude units.
use crate::message::Message;

// Guard against numeric overflow while allowing filtered full-scale transients.
pub(crate) const MAX_FILTERED_PCM: f32 = 32768.0 * 4.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Ft8,
    Ft4,
}
impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ft8 => "ft8",
            Self::Ft4 => "ft4",
        }
    }
    /// Slot spacing at 12 kHz: FT8 180,000 (15 s), FT4 90,000 (7.5 s).
    pub fn period_samples(self) -> usize {
        match self {
            Self::Ft8 => 180000,
            Self::Ft4 => 90000,
        }
    }
    /// Required decode window: FT8 180,000 samples; FT4 72,576 (6.048 s).
    pub fn input_samples(self) -> usize {
        match self {
            Self::Ft8 => 180000,
            Self::Ft4 => 72576,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DecodeSettings {
    pub low_hz: f32,
    pub high_hz: f32,
    pub priority_hz: Option<f32>,
    pub priority_tolerance_hz: f32,
    pub depth: u8,
    pub threads: usize,
    /// Master AP gate. Direct Rust callers must also set `ap.mode` to enable AP.
    pub assistance: bool,
    pub ap: crate::assistance::ApSettings,
}
impl Default for DecodeSettings {
    fn default() -> Self {
        Self {
            low_hz: 200.0,
            high_hz: 3000.0,
            priority_hz: None,
            priority_tolerance_hz: 10.0,
            depth: 3,
            threads: 1,
            assistance: false,
            ap: crate::assistance::ApSettings::default(),
        }
    }
}

/// Metrics of a successful error-correction attempt, not a measured channel BER.
#[derive(Clone, Debug)]
pub struct DecodeDiagnostics {
    pub method: crate::fec::DecodeMethod,
    pub bp_iterations: u8,
    pub hard_errors: usize,
    pub osd_pass: Option<u8>,
    /// One-based signal subtraction/search pass.
    pub pass: usize,
    pub ap_type: Option<u8>,
    pub confidence: Option<f32>,
    pub questionable: bool,
}
impl DecodeDiagnostics {
    pub(crate) fn bp(decoded: &crate::fec::Decoded) -> Self {
        Self {
            method: crate::fec::DecodeMethod::Bp,
            bp_iterations: decoded.iterations,
            hard_errors: decoded.hard_errors,
            osd_pass: None,
            pass: 1,
            ap_type: None,
            confidence: None,
            questionable: false,
        }
    }
    pub(crate) fn hybrid(decoded: &crate::fec::HybridDecoded) -> Self {
        Self {
            method: decoded.method,
            osd_pass: decoded.osd_pass,
            ..Self::bp(&decoded.decoded)
        }
    }
}

#[derive(Clone, Debug)]
pub struct DecodedSignal {
    /// Original decoded 77-bit payload, after mode-specific descrambling.
    pub source_bits: [bool; 77],
    pub message: Message,
    pub frequency_hz: f32,
    pub snr_db: i32,
    /// Offset from nominal mode transmit start, in seconds.
    pub dt_seconds: f32,
    pub assisted: bool,
    pub diagnostics: Option<DecodeDiagnostics>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DecodeStats {
    /// Candidate examinations, including revisits in subsequent passes.
    pub candidates: usize,
    pub passes: usize,
    pub decoded: usize,
    pub cancelled: bool,
}

impl DecodeSettings {
    pub fn ap_mode(&self) -> crate::assistance::ApMode {
        if self.assistance {
            self.ap.mode
        } else {
            crate::assistance::ApMode::Off
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if !self.low_hz.is_finite()
            || !self.high_hz.is_finite()
            || self.low_hz < 0.0
            || self.high_hz > 5000.0
            || self.high_hz - self.low_hz < 50.0
        {
            return Err(
                "frequency range must be finite, within 0..5000 Hz, and at least 50 Hz wide".into(),
            );
        }
        if self
            .priority_hz
            .is_some_and(|x| !x.is_finite() || !(0.0..=5000.0).contains(&x))
        {
            return Err("priority_hz must be finite and within 0..5000 Hz".into());
        }
        if !self.priority_tolerance_hz.is_finite()
            || !(0.0..=1000.0).contains(&self.priority_tolerance_hz)
        {
            return Err("priority_tolerance_hz must be within 0..1000 Hz".into());
        }
        if !(1..=3).contains(&self.depth) {
            return Err("depth must be 1, 2 or 3".into());
        }
        if !(1..=12).contains(&self.threads) {
            return Err("threads must be in 1..12".into());
        }
        self.ap.validate()?;
        Ok(())
    }
}

/// Per-stream decoder and callsign history. Reuse this for successive windows.
pub struct Decoder {
    mode: Mode,
    ft8: Option<crate::decode::ft8::Ft8Decoder>,
    ft4: Option<crate::decode::ft4::Ft4Decoder>,
    parallel_ft8: Option<crate::decode::parallel_ft8::ParallelFt8Decoder>,
    slot_context: Option<u64>,
    next_slot: u64,
    messages: crate::message::MessageDecoder,
    filtered_input: bool,
}
impl Decoder {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            ft8: None,
            ft4: None,
            parallel_ft8: None,
            slot_context: None,
            next_slot: 0,
            messages: crate::message::MessageDecoder::default(),
            filtered_input: false,
        }
    }
    pub(crate) fn new_for_wav(mode: Mode) -> Self {
        Self {
            filtered_input: true,
            ..Self::new(mode)
        }
    }
    pub fn reset(&mut self, mode: Mode) {
        self.mode = mode;
        self.messages.reset();
        self.ft8 = None;
        self.ft4 = None;
        self.parallel_ft8 = None;
        self.slot_context = None;
        self.next_slot = 0;
    }
    /// Set a period identity for history; repeat it for early/final attempts.
    pub fn set_slot(&mut self, slot: u64) {
        self.slot_context = Some(slot);
    }

    /// Decode one aligned window of mono 12 kHz audio synchronously.
    ///
    /// `pcm` must contain exactly [`Mode::input_samples`] finite raw PCM-scale
    /// samples in [-32768, 32768]. Convert normalized audio by multiplying by
    /// 32768. FT4 windows start every 90,000 samples but contain only 72,576.
    /// Invalid input/settings return an error; this method does not pad tails.
    ///
    /// Callbacks execute on the calling thread. With one worker they run as
    /// signals are found; parallel FT8 buffers and merges results after workers
    /// finish. Set `cancel` to true to request cooperative cancellation, including
    /// from a callback. In parallel mode callback cancellation can stop remaining
    /// delivery, but searches have already finished. Inspect `DecodeStats::cancelled`.
    ///
    /// Reuse the decoder for callsign/history continuity. Without [`Self::set_slot`],
    /// calls advance an implicit consecutive slot counter. Supply period identities
    /// when skipping slots or repeating a window. [`Self::reset`] clears history.
    pub fn decode(
        &mut self,
        pcm: &[f32],
        settings: &DecodeSettings,
        cancel: &std::sync::atomic::AtomicBool,
        on_decode: &mut dyn FnMut(DecodedSignal),
    ) -> Result<DecodeStats, String> {
        settings.validate()?;
        if self.mode == Mode::Ft4 && settings.threads != 1 {
            return Err("FT4 supports threads=1".into());
        }
        if pcm.len() != self.mode.input_samples() {
            return Err(format!(
                "{} requires {} samples",
                self.mode.name(),
                self.mode.input_samples()
            ));
        }
        if pcm.iter().any(|x| {
            !x.is_finite()
                || x.abs()
                    > if self.filtered_input {
                        MAX_FILTERED_PCM
                    } else {
                        32768.0
                    }
        }) {
            return Err("audio must contain finite PCM-scale samples".into());
        }
        for call in settings
            .ap
            .known_calls
            .iter()
            .chain(settings.ap.my_call.iter())
            .chain(settings.ap.dx_call.iter())
        {
            self.messages
                .remember_call(call)
                .map_err(|e| e.to_string())?;
        }
        let slot = self.slot_context.take().unwrap_or(self.next_slot);
        self.next_slot = slot.saturating_add(1);
        match self.mode {
            Mode::Ft8 if settings.threads > 1 => {
                let decoder = self.parallel_ft8.get_or_insert_with(|| {
                    let mut d = crate::decode::parallel_ft8::ParallelFt8Decoder::new();
                    d.filtered_input = self.filtered_input;
                    d
                });
                decoder.decode(
                    pcm,
                    settings,
                    &mut self.messages,
                    cancel,
                    on_decode,
                    Some(slot),
                )
            }
            Mode::Ft8 => {
                let decoder = self.ft8.get_or_insert_with(|| {
                    let mut d = crate::decode::ft8::Ft8Decoder::new();
                    d.filtered_input = self.filtered_input;
                    d
                });
                decoder.set_slot(slot);
                decoder.decode(pcm, settings, &mut self.messages, cancel, on_decode)
            }
            Mode::Ft4 => self
                .ft4
                .get_or_insert_with(|| {
                    let mut d = crate::decode::ft4::Ft4Decoder::new();
                    d.filtered_input = self.filtered_input;
                    d
                })
                .decode(pcm, settings, &mut self.messages, cancel, on_decode),
        }
    }
}
