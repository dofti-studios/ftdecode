// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded FT8 a7/a8 list decoding, ported from WSJT-X
//! ccdfaf3c1c109010d15399674ce278167cfde848 `ft8_a7.f90` and `ft8_a8d.f90`.
//! Slot identity is supplied by the stream owner; early/final calls must use
//! the same identity. No list history is advanced by calling `decode`.
use crate::{
    assistance::{Activity, ApMode},
    downsample::Ft8Downsampler,
    engine::{DecodeDiagnostics, DecodeSettings, DecodedSignal},
    fec,
    fft::{Complex32 as C, ComplexFft, Direction},
    message::MessageDecoder,
    message_encode, symbols,
};
use std::{
    f32::consts::PI,
    sync::atomic::{AtomicBool, Ordering},
};
const ZERO: C = C::new(0.0, 0.0);
const MAX_HISTORY: usize = 200;
const NWAVE: usize = 79 * 32;

#[derive(Clone)]
struct History {
    call1: String,
    call2: String,
    grid: String,
    frequency: f32,
    dt: f32,
    available: bool,
}
#[derive(Default)]
struct ParityHistory {
    slot: Option<u64>,
    current: Vec<History>,
    previous: Vec<History>,
}

/// Validated list result, with the waveform metadata needed for subtraction.
pub struct AssistedCandidate {
    pub signal: DecodedSignal,
    pub tones: [u8; 79],
    pub start: f32,
}

pub struct Ft8ListDecoder {
    slot: Option<u64>,
    history: [ParityHistory; 2],
    down: Ft8Downsampler,
    symbol: ComplexFft,
    dechirp: ComplexFft,
    waveform: ListWaveform,
}
impl Default for Ft8ListDecoder {
    fn default() -> Self {
        Self::new()
    }
}
impl Ft8ListDecoder {
    pub fn new() -> Self {
        Self {
            slot: None,
            history: Default::default(),
            down: Ft8Downsampler::new(),
            symbol: ComplexFft::new(32, Direction::Forward).unwrap(),
            dechirp: ComplexFft::new(3200, Direction::Forward).unwrap(),
            waveform: ListWaveform::new(),
        }
    }
    /// Invalidate station history without discarding reusable FFT plans.
    pub fn reset(&mut self) {
        self.slot = None;
        self.history = Default::default();
    }
    /// Identity counts 15-second FT8 periods, including periods not decoded.
    /// Backwards jumps and gaps beyond one opposite-parity period
    /// invalidate history. Repeating an identity is an early/final update.
    pub fn begin_slot(&mut self, slot: u64) {
        if self.slot == Some(slot) {
            return;
        }
        if self.slot.is_some_and(|old| slot < old || slot - old > 2) {
            self.reset();
        }
        let h = &mut self.history[(slot % 2) as usize];
        h.previous = if h.slot.is_some_and(|old| old.checked_add(2) == Some(slot)) {
            std::mem::take(&mut h.current)
        } else {
            Vec::new()
        };
        for entry in &mut h.previous {
            entry.available = true;
        }
        h.current.clear();
        h.slot = Some(slot);
        self.slot = Some(slot);
    }
    /// Save only standard, fully resolved call pairs, as the native a7 saver
    /// does. Newly decoded stations suppress their earlier hypotheses.
    pub fn remember(&mut self, signal: &DecodedSignal) {
        let Some(slot) = self.slot else {
            return;
        };
        if !(0.0..=6000.0).contains(&signal.frequency_hz)
            || !(-15.0..=15.0).contains(&signal.dt_seconds)
        {
            return;
        }
        let text = &signal.message.text;
        if text.contains(['/', '<', '>']) || text.starts_with("CQ_") {
            return;
        }
        let words: Vec<_> = text.split_whitespace().collect();
        if words.len() < 2
            || (words[0] != "CQ" && !standard_call(words[0]))
            || !standard_call(words[1])
        {
            return;
        }
        let entry = History {
            call1: words[0].into(),
            call2: words[1].into(),
            grid: words
                .last()
                .filter(|w| grid4(w))
                .map_or(String::new(), |w| w.to_string()),
            frequency: signal.frequency_hz,
            dt: signal.dt_seconds,
            available: true,
        };
        let h = &mut self.history[(slot % 2) as usize];
        for old in &mut h.previous {
            if old.call2 == entry.call2 && (old.frequency - entry.frequency).abs() <= 3.0 {
                old.available = false;
            }
        }
        if h.current.len() < MAX_HISTORY
            && !h.current.iter().any(|old| {
                old.call1 == entry.call1
                    && old.call2 == entry.call2
                    && (old.frequency - entry.frequency).abs() <= 3.0
            })
        {
            h.current.push(entry);
        }
    }
    pub fn decode(
        &mut self,
        pcm: &[f32],
        settings: &DecodeSettings,
        messages: &mut MessageDecoder,
        cancel: &AtomicBool,
    ) -> Result<Vec<AssistedCandidate>, String> {
        self.decode_with_baseline(pcm, settings, messages, cancel, None)
    }
    /// `baseline` is the linear acquisition noise baseline indexed at 3.125 Hz.
    /// Without one a7 reports the native minimum SNR; its acceptance is unchanged.
    pub fn decode_with_baseline(
        &mut self,
        pcm: &[f32],
        settings: &DecodeSettings,
        messages: &mut MessageDecoder,
        cancel: &AtomicBool,
        baseline: Option<&[f32]>,
    ) -> Result<Vec<AssistedCandidate>, String> {
        settings.validate()?;
        if pcm.len() != 180000 || pcm.iter().any(|s| !s.is_finite() || s.abs() > 1.0e12) {
            return Err("FT8 list decoding requires 180000 finite residual samples".into());
        }
        let mut result = Vec::new();
        if settings.ap_mode() != ApMode::Auto
            || matches!(settings.ap.activity, Activity::Fox | Activity::Hound)
            || cancel.load(Ordering::Relaxed)
        {
            return Ok(result);
        }
        let history = self
            .slot
            .map(|slot| self.history[(slot % 2) as usize].previous.clone())
            .unwrap_or_default();
        let target = settings
            .priority_hz
            .zip(settings.ap.my_call.as_deref())
            .zip(settings.ap.dx_call.as_deref())
            .zip(settings.ap.dx_grid.as_deref());
        if history.iter().all(|h| !h.available) && target.is_none() {
            return Ok(result);
        }
        if pcm.iter().all(|s| *s == 0.0) {
            return Ok(result);
        }
        self.down.prepare_residual(pcm).map_err(|e| e.to_string())?;
        for (index, h) in history.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(result);
            }
            // Saving an a7 result can suppress later entries from this same
            // transmitter. Consult the live table, not the cloned snapshot.
            if self
                .slot
                .is_some_and(|slot| !self.history[(slot % 2) as usize].previous[index].available)
            {
                continue;
            }
            if !(settings.low_hz - 2.5..=settings.high_hz + 2.5).contains(&h.frequency) {
                continue;
            }
            if let Some(candidate) = self.a7(h, baseline, settings, messages, cancel)? {
                self.remember(&candidate.signal);
                result.push(candidate);
            }
        }
        if let Some((((frequency, mycall), dxcall), grid)) = target {
            let already = self.slot.is_some_and(|slot| {
                self.history[(slot % 2) as usize]
                    .current
                    .iter()
                    .any(|h| h.call2 == dxcall || h.call1 == dxcall)
            });
            if !already
                && !cancel.load(Ordering::Relaxed)
                && (settings.low_hz - 5.0..=settings.high_hz + 5.0).contains(&frequency)
                && let Some(candidate) =
                    self.a8(frequency, mycall, dxcall, grid, settings, messages, cancel)?
            {
                self.remember(&candidate.signal);
                result.push(candidate);
            }
        }
        Ok(result)
    }
    fn a7(
        &mut self,
        h: &History,
        baseline: Option<&[f32]>,
        settings: &DecodeSettings,
        messages: &mut MessageDecoder,
        cancel: &AtomicBool,
    ) -> Result<Option<AssistedCandidate>, String> {
        let mut data = [ZERO; 3200];
        self.down
            .extract(h.frequency, &mut data)
            .map_err(|e| e.to_string())?;
        let initial = ((h.dt + 0.5) * 200.0).round() as i32;
        let mut best = initial;
        let mut peak = 0.0;
        for start in initial - 10..=initial + 10 {
            let score = super::ft8::sync(&data, start, 0.0);
            if score > peak {
                peak = score;
                best = start;
            }
        }
        let mut delta = 0.0;
        peak = 0.0;
        for f in -5..=5 {
            let score = super::ft8::sync(&data, best, f as f32 * 0.5);
            if score > peak {
                peak = score;
                delta = f as f32 * 0.5;
            }
        }
        let frequency = h.frequency + delta;
        if !(settings.low_hz..=settings.high_hz).contains(&frequency) {
            return Ok(None);
        }
        self.down
            .extract(frequency, &mut data)
            .map_err(|e| e.to_string())?;
        let mut fine = best;
        peak = 0.0;
        for start in best - 4..=best + 4 {
            let score = super::ft8::sync(&data, start, 0.0);
            if score > peak {
                peak = score;
                fine = start;
            }
        }
        let mut spectra = [[ZERO; 8]; 79];
        let mut work = [ZERO; 32];
        for (k, sym) in spectra.iter_mut().enumerate() {
            work.fill(ZERO);
            let begin = fine + k as i32 * 32;
            if begin >= 0 && begin + 31 < 2812 {
                work.copy_from_slice(&data[begin as usize..begin as usize + 32]);
            }
            self.symbol
                .transform(&mut work)
                .map_err(|e| e.to_string())?;
            for t in 0..8 {
                sym[t] = work[t] / 1000.0;
            }
        }
        let metrics = super::ft8::metrics(&spectra, false);
        // Out-of-window history or underflow can leave no local evidence even
        // when the input recording is nonzero. In that case every hypothesis
        // has distance zero, so the usual runner-up ratio cannot select one.
        if metrics[..4].iter().flatten().all(|&value| value == 0.0) {
            return Ok(None);
        }
        let mut ranked: Vec<(f32, usize, Hypothesis)> = Vec::with_capacity(206);
        for hypothesis in hypotheses(&h.call1, &h.call2, &h.grid, false) {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let mut distance = f32::INFINITY;
            let mut errors = 0;
            for metric in metrics.iter().take(4) {
                let mut d = 0.0;
                let mut n = 0;
                for (i, &bit) in hypothesis.codeword.iter().enumerate() {
                    if (metric[i] >= 0.0) != bit {
                        d += metric[i].abs();
                        if metric[i] != 0.0 {
                            n += 1;
                        }
                    }
                }
                if d < distance {
                    distance = d;
                    errors = n;
                }
            }
            ranked.push((distance, errors, hypothesis));
        }
        ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
        if ranked.len() < 2 {
            return Ok(None);
        }
        let second = ranked[1].0;
        let (distance, errors, hypothesis) = ranked.remove(0);
        if !distance.is_finite()
            || distance > 100.0
            || second < 1.3 * distance
            || hypothesis.text.starts_with("QU1RK ")
            || (hypothesis.text.starts_with("CQ ") && h.grid.is_empty())
        {
            return Ok(None);
        }
        let mut snr = -25.0;
        if let Some(base) = baseline
            .and_then(|b| b.get((h.frequency / 3.125).round().max(1.0) as usize))
            .filter(|&&b| b > 0.0)
        {
            let power = hypothesis
                .tones
                .iter()
                .enumerate()
                .map(|(i, &t)| spectra[i][t as usize].norm_sqr() * 1e6)
                .sum::<f32>();
            let arg = power / base / 3e6 - 1.0;
            if arg > 0.0 {
                snr = (10.0 * arg.log10() - 27.0).max(-25.0);
            }
        }
        make_candidate(
            hypothesis,
            messages,
            frequency,
            (fine - 1) as f32 / 200.0 - 0.5,
            snr as i32,
            7,
            errors,
            1.0,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn a8(
        &mut self,
        frequency: f32,
        mycall: &str,
        dxcall: &str,
        grid: &str,
        settings: &DecodeSettings,
        messages: &mut MessageDecoder,
        cancel: &AtomicBool,
    ) -> Result<Option<AssistedCandidate>, String> {
        let mut data = [ZERO; 3200];
        self.down
            .extract(frequency, &mut data)
            .map_err(|e| e.to_string())?;
        let mut work = [ZERO; 3200];
        let mut spectrum = [0.0; 3201];
        let mut best_spectrum = [0.0; 3201];
        let mut best_power = 0.0;
        let mut best_frequency = 0.0;
        let mut best_lag = 0;
        let mut best_hypothesis = None;
        for hypothesis in hypotheses(mycall, dxcall, &grid[..4], true) {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            self.waveform.generate(&hypothesis.tones);
            let wave = &self.waveform.samples;
            let mut peak = 0.0;
            let mut peak_lag = 0;
            let mut peak_frequency = 0.0;
            let mut peak_spectrum = [0.0; 3201];
            for iteration in 0..2 {
                let (first, last, step) = if iteration == 0 {
                    (-200, 200, 4)
                } else {
                    (peak_lag - 8, peak_lag + 8, 1)
                };
                for lag in (first..=last).step_by(step) {
                    if cancel.load(Ordering::Relaxed) {
                        return Ok(None);
                    }
                    work.fill(ZERO);
                    for i in 0..NWAVE {
                        let j = i as i32 + lag + 100;
                        // This truncation to NWAVE is intentional: native a8
                        // ignores the downsampled tail during its lag search.
                        if (0..NWAVE as i32).contains(&j) {
                            work[i] = data[j as usize] * wave[i].conj();
                        }
                    }
                    self.dechirp
                        .transform(&mut work)
                        .map_err(|e| e.to_string())?;
                    spectrum[0] = 0.0;
                    for (i, value) in work.iter().enumerate() {
                        let bin = if i <= 1600 { i + 1600 } else { i - 1600 };
                        spectrum[bin] = 1e-6 * value.norm_sqr();
                    }
                    let mut old = spectrum[0];
                    for i in 1..3200 {
                        let value = spectrum[i];
                        spectrum[i] = 0.5 * value + 0.25 * (old + spectrum[i + 1]);
                        old = value;
                    }
                    for (i, &power) in spectrum.iter().enumerate() {
                        if power > peak {
                            peak = power;
                            peak_frequency = frequency + (i as f32 - 1600.0) * 0.0625;
                            peak_lag = lag;
                            peak_spectrum = spectrum;
                        }
                    }
                }
            }
            if peak > best_power {
                best_power = peak;
                best_frequency = peak_frequency;
                best_lag = peak_lag;
                best_spectrum = peak_spectrum;
                best_hypothesis = Some(hypothesis);
            }
        }
        let Some(hypothesis) = best_hypothesis else {
            return Ok(None);
        };
        if hypothesis.text.is_empty()
            || (frequency - best_frequency).abs() > 5.0
            || !(settings.low_hz..=settings.high_hz).contains(&best_frequency)
        {
            return Ok(None);
        }
        let average = (best_spectrum[1400..=1500].iter().sum::<f32>()
            + best_spectrum[1700..=1800].iter().sum::<f32>())
            / 202.0;
        if average <= 0.0 || !average.is_finite() {
            return Ok(None);
        }
        for power in &mut best_spectrum {
            *power = *power / average - 1.0;
        }
        let peak = best_spectrum[1568..=1632]
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        let mut signal_power = 0.0;
        let mut count = 0;
        for &power in &best_spectrum[1568..=1632] {
            if power >= 0.5 * peak {
                signal_power += power;
                count += 1;
            }
        }
        let snr = if count > 0 && signal_power > 0.0 {
            (10.0 * (signal_power / count as f32).log10() - 35.0).max(-30.0)
        } else {
            -30.0
        };
        // Native twkfreq1 advances the phase before multiplying each sample.
        let step = C::from_polar(1.0, 2.0 * PI * (frequency - best_frequency) / 200.0);
        let mut phase = C::new(1.0, 0.0);
        for sample in &mut data {
            phase *= step;
            *sample *= phase;
        }
        let mut plog = 0.0;
        let mut hard = 0;
        let mut total = 0.0;
        let mut biggest = 0.0;
        let mut sym = [ZERO; 32];
        for (k, &tone) in hypothesis.tones.iter().enumerate() {
            sym.fill(ZERO);
            let first = k as i32 * 32 + best_lag + 100;
            for (i, sample) in sym.iter_mut().enumerate() {
                let j = first + i as i32;
                if (0..3200).contains(&j) {
                    *sample = data[j as usize];
                }
            }
            self.symbol.transform(&mut sym).map_err(|e| e.to_string())?;
            let powers: [f32; 8] = std::array::from_fn(|i| sym[i].norm_sqr());
            let sum = powers.iter().sum::<f32>();
            let winner = (0..8)
                .max_by(|&a, &b| powers[a].total_cmp(&powers[b]).then_with(|| b.cmp(&a)))
                .unwrap();
            plog += if sum > 0.0 {
                (powers[tone as usize] / sum).ln()
            } else {
                0.125f32.ln()
            };
            hard += usize::from(winner != tone as usize);
            total += powers[tone as usize];
            biggest += powers[winner];
        }
        if !plog.is_finite()
            || hard > 54
            || plog < -159.0
            || biggest <= 0.0
            || total / biggest < 0.71
        {
            return Ok(None);
        }
        for call in [mycall, dxcall] {
            messages
                .remember_call(call.trim_matches(['<', '>']))
                .map_err(|e| e.to_string())?;
        }
        make_candidate(
            hypothesis,
            messages,
            best_frequency,
            best_lag as f32 * 0.005,
            snr.round() as i32,
            8,
            hard,
            if plog < -147.0 { 0.16 } else { 1.0 },
        )
    }
}

struct Hypothesis {
    text: String,
    bits: [bool; 77],
    codeword: [bool; 174],
    tones: [u8; 79],
}
fn grid4(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 4
        && b[..2].iter().all(|b| (b'A'..=b'R').contains(b))
        && b[2..].iter().all(u8::is_ascii_digit)
        && s != "RR73"
}
fn standard_call(call: &str) -> bool {
    // Native stdcall (q65_set_list.f90), which is intentionally less strict
    // than the 28-bit packer for compound calls. It selects list message form.
    let call = call.to_ascii_uppercase();
    let b = call.as_bytes();
    let Some(area) = b.iter().rposition(u8::is_ascii_digit) else {
        return false;
    };
    (1..=2).contains(&area)
        && b[..area].iter().any(u8::is_ascii_alphabetic)
        && b[area + 1..]
            .iter()
            .filter(|b| b.is_ascii_alphabetic())
            .count()
            <= 3
}
fn hypotheses(call1: &str, call2: &str, grid: &str, a8: bool) -> Vec<Hypothesis> {
    let first_std = call1 == "CQ" || standard_call(call1);
    let second_std = standard_call(call2);
    let mut result = Vec::with_capacity(206);
    for i in 1..=206 {
        let mut prefix = format!("{call1} {call2}");
        if !a8 && call1 == "CQ" && i != 5 {
            prefix = format!("QU1RK {call2}");
        }
        if !first_std {
            if i == 1 || i >= 6 {
                prefix = format!("<{}> {call2}", call1.trim_matches(['<', '>']));
            }
            if (2..=4).contains(&i) {
                prefix = format!("{call1} <{}>", call2.trim_matches(['<', '>']));
            }
        } else if !second_std {
            if i <= 4 || i == 6 {
                prefix = format!("<{call1}> {call2}");
            }
            if i >= 7 {
                prefix = format!("{call1} <{}>", call2.trim_matches(['<', '>']));
            }
        }
        let mut text = match i {
            1 => prefix,
            2 => format!("{prefix} RRR"),
            3 => format!("{prefix} RR73"),
            4 => format!("{prefix} 73"),
            5 => {
                if second_std && grid != "RR73" {
                    format!("CQ {call2} {grid}").trim_end().to_owned()
                } else {
                    format!("CQ {call2}")
                }
            }
            6 => {
                if second_std {
                    format!("{prefix} {grid}").trim_end().to_owned()
                } else {
                    prefix
                }
            }
            _ => {
                let report: i32 = -50 + (i - 7) / 2;
                if a8 && report.abs() > 30 {
                    String::new()
                } else {
                    format!("{prefix} {}{report:+03}", if i % 2 == 0 { "R" } else { "" })
                }
            }
        };
        // getmsg intentionally creates a blank hypothesis for reports outside
        // +/-30. It may win the native waveform search but cannot be returned.
        let bits = if text.is_empty() {
            // pack77 classifies the blank getmsg result as zero telemetry,
            // i3=0/n3=5. Its waveform still competes, then a blank winner is
            // rejected. Using zero free-text bits changes this noise gate.
            let mut bits = [false; 77];
            bits[71] = true;
            bits[73] = true;
            bits
        } else {
            match message_encode::encode(&text) {
                Ok(bits) => bits,
                Err(_) if a8 && i == 1 && second_std && call2.contains('/') => {
                    // Native stdcall treats short portable calls (K1A/P)
                    // as standard, but its bare two-call type-4 packing
                    // leaves an empty full-call field. Keep the pinned
                    // oracle's zero-field waveform as a rejecting competitor.
                    // Never return this malformed message as a decode.
                    text.clear();
                    let mut bits = [false; 77];
                    bits[74] = true;
                    bits
                }
                Err(_) => continue,
            }
        };
        let codeword = fec::encode(&bits);
        let tones = symbols::ft8(&codeword);
        result.push(Hypothesis {
            text,
            bits,
            codeword,
            tones,
        });
    }
    result
}
#[allow(clippy::too_many_arguments)]
fn make_candidate(
    h: Hypothesis,
    messages: &mut MessageDecoder,
    frequency: f32,
    dt: f32,
    snr: i32,
    ap: u8,
    errors: usize,
    confidence: f32,
) -> Result<Option<AssistedCandidate>, String> {
    let Ok(message) = messages.unpack(&h.bits) else {
        return Ok(None);
    };
    Ok(Some(AssistedCandidate {
        signal: DecodedSignal {
            source_bits: h.bits,
            message,
            frequency_hz: frequency,
            snr_db: snr,
            dt_seconds: dt,
            assisted: true,
            diagnostics: Some(DecodeDiagnostics {
                method: fec::DecodeMethod::List,
                bp_iterations: 0,
                hard_errors: errors,
                osd_pass: None,
                pass: 1,
                ap_type: Some(ap),
                confidence: Some(confidence),
                questionable: confidence < 0.2,
            }),
        },
        tones: h.tones,
        start: dt + 0.5,
    }))
}

struct ListWaveform {
    pulse: [f32; 96],
    table: Vec<C>,
    samples: [C; NWAVE],
}
impl ListWaveform {
    fn new() -> Self {
        let pulse = std::array::from_fn(|i| {
            let t = (i as f32 + 1.0 - 48.0) / 32.0;
            let c = PI * (2.0 / 2.0f32.ln()).sqrt() * 2.0;
            0.5 * (erf(c * (t + 0.5)) - erf(c * (t - 0.5)))
        });
        Self {
            pulse,
            table: (0..=65536)
                .map(|i| C::from_polar(1.0, i as f32 * 2.0 * PI / 65536.0))
                .collect(),
            samples: [ZERO; NWAVE],
        }
    }
    fn generate(&mut self, tones: &[u8; 79]) {
        let mut increments = [0.0; 81 * 32];
        let scale = 2.0 * PI / 32.0;
        for (s, &tone) in tones.iter().enumerate() {
            for (i, &p) in self.pulse.iter().enumerate() {
                increments[s * 32 + i] += scale * p * tone as f32;
            }
        }
        for i in 0..64 {
            increments[i] += scale * tones[0] as f32 * self.pulse[32 + i];
            increments[79 * 32 + i] += scale * tones[78] as f32 * self.pulse[i];
        }
        let mut phase = 0.0;
        for (out, &step) in self.samples.iter_mut().zip(&increments[32..80 * 32]) {
            *out = self.table[(phase * 65536.0 / (2.0 * PI)) as usize];
            phase = (phase + step) % (2.0 * PI);
        }
        for i in 0..4 {
            self.samples[i] *= 0.5 * (1.0 - (PI * i as f32 / 4.0).cos());
            self.samples[NWAVE - 4 + i] *= 0.5 * (1.0 + (PI * i as f32 / 4.0).cos());
        }
    }
}
fn erf(x: f32) -> f32 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let poly =
        ((((1.0614054 * t - 1.4531521) * t + 1.4214138) * t - 0.28449672) * t + 0.2548296) * t;
    (1.0 - poly * (-x * x).exp()).copysign(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn station(frequency: f32) -> DecodedSignal {
        let bits = message_encode::encode("CQ K1ABC FN42").unwrap();
        DecodedSignal {
            source_bits: bits,
            message: MessageDecoder::default().unpack(&bits).unwrap(),
            frequency_hz: frequency,
            snr_db: -10,
            dt_seconds: 0.0,
            assisted: false,
            diagnostics: None,
        }
    }
    #[test]
    fn native_a8_hypotheses_include_standard_and_compound_calls() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/ft8-list-hypotheses.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let candidates = hypotheses(
                case["call1"].as_str().unwrap(),
                case["call2"].as_str().unwrap(),
                case["grid"].as_str().unwrap(),
                true,
            );
            let entries = case["entries"].as_array().unwrap();
            assert_eq!(
                candidates.len(),
                entries.len(),
                "{} {} missing {:?}",
                case["call1"],
                case["call2"],
                entries
                    .iter()
                    .filter(|e| !candidates
                        .iter()
                        .any(|c| Some(c.text.as_str()) == e["message"].as_str()))
                    .collect::<Vec<_>>()
            );
            for (candidate, expected) in candidates.iter().zip(entries) {
                let bits: String = candidate
                    .bits
                    .iter()
                    .map(|b| if *b { '1' } else { '0' })
                    .collect();
                assert_eq!(
                    bits,
                    expected["bits"].as_str().unwrap(),
                    "{} {} {}",
                    case["call1"],
                    case["call2"],
                    expected["index"]
                );
            }
        }
    }
    #[test]
    fn history_is_bounded_for_both_parities() {
        let mut decoder = Ft8ListDecoder::new();
        for slot in 0..4 {
            decoder.begin_slot(slot);
            for i in 0..250 {
                decoder.remember(&station(300.0 + i as f32 * 4.0));
            }
            assert_eq!(
                decoder.history[(slot % 2) as usize].current.len(),
                MAX_HISTORY
            );
        }
        assert!(
            decoder
                .history
                .iter()
                .all(|h| h.current.len() == MAX_HISTORY && h.previous.len() == MAX_HISTORY)
        );
        decoder.reset();
        assert!(
            decoder
                .history
                .iter()
                .all(|h| h.current.is_empty() && h.previous.is_empty())
        );
    }
    #[test]
    fn unknown_and_compound_calls_do_not_seed_a7() {
        let mut decoder = Ft8ListDecoder::new();
        decoder.begin_slot(0);
        let mut signal = station(1500.0);
        for text in ["CQ <...> FN42", "CQ PJ4/K1ABC", "CQ DX K1ABC FN42"] {
            signal.message.text = text.into();
            decoder.remember(&signal);
        }
        assert!(decoder.history[0].current.is_empty());
    }
}
