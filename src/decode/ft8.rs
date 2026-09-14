// SPDX-License-Identifier: GPL-3.0-or-later
//! FT8 receive path, ported from WSJT-X ccdfaf3c1c109010d15399674ce278167cfde848.
//! Standard AP-off acquisition, coherent metrics, BP/OSD and successive cancellation.
use crate::{
    downsample::Ft8Downsampler,
    engine::{DecodeDiagnostics, DecodeSettings, DecodeStats, DecodedSignal},
    fec,
    fft::{Complex32 as C, ComplexFft, Direction, RealForward},
    message::MessageDecoder,
    symbols,
};
use std::{
    f32::consts::PI,
    sync::atomic::{AtomicBool, Ordering},
};
const COSTAS: [usize; 7] = [3, 1, 4, 0, 6, 5, 2];
const GRAY: [usize; 8] = [0, 1, 3, 2, 5, 6, 4, 7];
const ZERO: C = C::new(0.0, 0.0);
#[derive(Clone, Copy)]
struct Candidate {
    frequency: f32,
    start: f32,
    score: f32,
}
/// WSJT-X's disk driver supplies successively longer, zero-padded windows.
/// The public decoder still requires a complete slot before running these stages.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AcquisitionStage {
    Early,
    Final,
    Full,
}

#[derive(Clone, Copy)]
struct SavedSignal {
    tones: [u8; 79],
    frequency: f32,
    start: f32,
}

type SignalIdentity = ([bool; 77], f32, f32);

fn already_seen(seen: &[SignalIdentity], signal: &DecodedSignal, start: f32) -> bool {
    same_signal(seen, signal, start, 4.0)
}

fn already_emitted(seen: &[SignalIdentity], signal: &DecodedSignal, start: f32) -> bool {
    // Coarse subtraction can move the refined peak within one FT8 tone bin.
    // Report that signal once, while retaining distinct same-message carriers.
    same_signal(seen, signal, start, 6.25)
}

fn same_signal(seen: &[SignalIdentity], signal: &DecodedSignal, start: f32, width: f32) -> bool {
    seen.iter().any(|(bits, frequency, time)| {
        *bits == signal.source_bits
            && (frequency - signal.frequency_hz).abs() < width
            && (time - start).abs() < 0.08
    })
}

pub struct Ft8Decoder {
    pub(crate) filtered_input: bool,
    down: Ft8Downsampler,
    coarse: RealForward,
    symbol: ComplexFft,
    subtraction: Option<Subtractor>,
    lists: Option<super::ft8_assistance::Ft8ListDecoder>,
    slot: Option<u64>,
    next_slot: u64,
}
impl Default for Ft8Decoder {
    fn default() -> Self {
        Self::new()
    }
}
impl Ft8Decoder {
    pub fn new() -> Self {
        Self {
            filtered_input: false,
            down: Ft8Downsampler::new(),
            coarse: RealForward::new(3840).unwrap(),
            symbol: ComplexFft::new(32, Direction::Forward).unwrap(),
            subtraction: None,
            lists: None,
            slot: None,
            next_slot: 0,
        }
    }
    /// Set the period identity; early/final attempts for one slot share it.
    pub fn set_slot(&mut self, slot: u64) {
        self.slot = Some(slot);
    }

    pub fn decode(
        &mut self,
        pcm: &[f32],
        settings: &DecodeSettings,
        messages: &mut MessageDecoder,
        cancel: &AtomicBool,
        on_decode: &mut dyn FnMut(DecodedSignal),
    ) -> Result<DecodeStats, String> {
        if pcm.len() != 180000
            || pcm.iter().any(|x| {
                !x.is_finite()
                    || x.abs()
                        > if self.filtered_input {
                            crate::engine::MAX_FILTERED_PCM
                        } else {
                            32768.0
                        }
            })
        {
            return Err("FT8 requires 180000 finite PCM samples in [-32768,32768]".into());
        }
        settings.validate()?;
        let slot = self.slot.take().unwrap_or(self.next_slot);
        self.next_slot = slot.saturating_add(1);
        if settings.ap_mode() == crate::assistance::ApMode::Auto || self.lists.is_some() {
            self.lists
                .get_or_insert_with(super::ft8_assistance::Ft8ListDecoder::new)
                .begin_slot(slot);
        }
        let mut stats = DecodeStats::default();
        let mut residual = pcm.to_vec();
        let mut early = Vec::new();
        let mut emitted = Vec::new();
        // Subtraction identities belong to a residual, while callback identities
        // span all stages. The independent full-slot fallback must subtract its
        // own strong signals even when an earlier stage already emitted them.
        let mut residual_seen = Vec::new();
        let stages: &[AcquisitionStage] = if settings.depth == 1 {
            &[AcquisitionStage::Full]
        } else {
            &[
                AcquisitionStage::Early,
                AcquisitionStage::Final,
                AcquisitionStage::Full,
            ]
        };
        for &stage in stages {
            if cancelled(cancel, &mut stats) {
                break;
            }
            residual.copy_from_slice(pcm);
            match stage {
                AcquisitionStage::Early => residual[41 * 3456..].fill(0.0),
                AcquisitionStage::Final => {
                    self.resume_early_residual(&mut residual, pcm, &early, settings, cancel)?;
                }
                AcquisitionStage::Full => {
                    // The full-slot metric fallback recovers signals which can
                    // be lost with estimates made from the truncated early data.
                    residual_seen.clear();
                }
            }
            let mut last_baseline = Vec::new();
            for pass in 0..if settings.depth == 1 { 2 } else { 3 } {
                if cancelled(cancel, &mut stats) {
                    break;
                }
                if pass == 2 && residual_seen.is_empty() {
                    break;
                }
                stats.passes += 1;
                let (candidates, baseline) = self.acquire(&residual, settings, cancel)?;
                if cancelled(cancel, &mut stats) {
                    break;
                }
                self.down
                    .prepare_residual(&residual)
                    .map_err(|e| e.to_string())?;
                for candidate in candidates {
                    if cancelled(cancel, &mut stats) {
                        break;
                    }
                    stats.candidates += 1;
                    if let Some((mut signal, tones, start)) = self.candidate(
                        candidate,
                        settings,
                        pass > 0,
                        stage,
                        &baseline,
                        messages,
                        cancel,
                    )? {
                        let duplicate = already_seen(&residual_seen, &signal, start);
                        if stage == AcquisitionStage::Full && duplicate {
                            continue;
                        }
                        if !duplicate {
                            residual_seen.push((signal.source_bits, signal.frequency_hz, start));
                        }
                        let frequency = signal.frequency_hz;
                        if !already_emitted(&emitted, &signal, start) {
                            emitted.push((signal.source_bits, frequency, start));
                            stats.decoded += 1;
                            if let Some(d) = &mut signal.diagnostics {
                                d.pass = pass + 1;
                            }
                            if let Some(lists) = &mut self.lists {
                                lists.remember(&signal);
                            }
                            on_decode(signal);
                        }
                        if cancelled(cancel, &mut stats) {
                            break;
                        }
                        let saved = SavedSignal {
                            tones,
                            frequency,
                            start,
                        };
                        if stage == AcquisitionStage::Early && !duplicate {
                            early.push(saved);
                        }
                        // Native early/final searches coarse-subtract every
                        // accepted candidate, including repeated decodes. The
                        // stage transition refines the unique saved signals.
                        self.subtract(
                            &mut residual,
                            &saved,
                            stage == AcquisitionStage::Full && settings.depth == 3,
                            cancel,
                        )?;
                    }
                }
                last_baseline = baseline;
            }
            if stage != AcquisitionStage::Early
                && !cancelled(cancel, &mut stats)
                && let Some(lists) = &mut self.lists
            {
                let results = lists.decode_with_baseline(
                    &residual,
                    settings,
                    messages,
                    cancel,
                    (!last_baseline.is_empty()).then_some(last_baseline.as_slice()),
                )?;
                for mut candidate in results {
                    if cancelled(cancel, &mut stats) {
                        break;
                    }
                    if already_emitted(&emitted, &candidate.signal, candidate.start) {
                        continue;
                    }
                    emitted.push((
                        candidate.signal.source_bits,
                        candidate.signal.frequency_hz,
                        candidate.start,
                    ));
                    stats.decoded += 1;
                    if let Some(d) = &mut candidate.signal.diagnostics {
                        d.pass = stats.passes;
                    }
                    on_decode(candidate.signal);
                }
            }
        }
        stats.cancelled |= cancel.load(Ordering::Relaxed);
        Ok(stats)
    }
    /// Stage 47 reloads the longer prefix and subtracts early decodes whose
    /// complete waveform is available. Stage 50 retains that residual, appends
    /// fresh samples and then subtracts late-starting early decodes.
    fn resume_early_residual(
        &mut self,
        residual: &mut [f32],
        pcm: &[f32],
        early: &[SavedSignal],
        settings: &DecodeSettings,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        residual[47 * 3456..].fill(0.0);
        for signal in early {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            if signal.start - 0.5 < 0.396 {
                self.subtract(residual, signal, settings.depth == 3, cancel)?;
            }
        }
        residual[47 * 3456..50 * 3456].copy_from_slice(&pcm[47 * 3456..50 * 3456]);
        for signal in early {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            if signal.start - 0.5 >= 0.396 {
                self.subtract(residual, signal, true, cancel)?;
            }
        }
        Ok(())
    }

    fn subtract(
        &mut self,
        pcm: &mut [f32],
        signal: &SavedSignal,
        refined: bool,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let subtraction = self.subtraction.get_or_insert_with(Subtractor::new);
        if refined {
            subtraction.subtract_refined(pcm, &signal.tones, signal.frequency, signal.start, cancel)
        } else {
            subtraction.subtract(pcm, &signal.tones, signal.frequency, signal.start, cancel)
        }
    }
    fn acquire(
        &mut self,
        pcm: &[f32],
        settings: &DecodeSettings,
        cancel: &AtomicBool,
    ) -> Result<(Vec<Candidate>, Vec<f32>), String> {
        let mut spectra = vec![0.0f32; 372 * 1921];
        let mut input = vec![0.0; 3840];
        let mut out = vec![ZERO; 1921];
        for j in 0..372 {
            if cancel.load(Ordering::Relaxed) {
                return Ok((Vec::new(), Vec::new()));
            }
            input.fill(0.0);
            for i in 0..1920 {
                input[i] = pcm[j * 480 + i] / 300.0;
            }
            self.coarse
                .transform(&mut input, &mut out)
                .map_err(|e| e.to_string())?;
            for i in 0..1921 {
                spectra[j * 1921 + i] = out[i].norm_sqr();
            }
        }
        let low = ((settings.low_hz / 3.125).round() as usize).max(1);
        let high = ((settings.high_hz / 3.125).round() as usize).min(1906);
        let mut peaks = Vec::new();
        let mut normal = Vec::new();
        let mut extended = Vec::new();
        for bin in low..=high {
            if cancel.load(Ordering::Relaxed) {
                return Ok((Vec::new(), Vec::new()));
            }
            let mut best = [(0.0f32, 0i32); 2];
            for lag in -62i32..=62 {
                let mut power = [0.0; 3];
                let mut total = [0.0; 3];
                for block in 0..3 {
                    for (n, &tone) in COSTAS.iter().enumerate() {
                        let frame = lag + 12 + 4 * n as i32 + 144 * block as i32 - 1;
                        if !(0..372).contains(&frame) {
                            continue;
                        }
                        let row = frame as usize * 1921 + bin;
                        power[block] += spectra[row + 2 * tone];
                        for t in 0..7 {
                            total[block] += spectra[row + 2 * t];
                        }
                    }
                }
                let a = power.iter().sum::<f32>();
                let b = total.iter().sum::<f32>();
                let p = power[1] + power[2];
                let t = total[1] + total[2];
                let score = (6.0 * a / (b - a).max(1e-30)).max(6.0 * p / (t - p).max(1e-30));
                if lag.abs() <= 13 && score > best[0].0 {
                    best[0] = (score, lag)
                }
                if score > best[1].0 {
                    best[1] = (score, lag)
                }
            }
            normal.push(best[0].0);
            extended.push(best[1].0);
            peaks.push((bin, best));
        }
        if normal.is_empty() {
            return Ok((Vec::new(), vec![0.0; 1921]));
        }
        normal.sort_by(f32::total_cmp);
        extended.sort_by(f32::total_cmp);
        let base = [
            normal[(normal.len() * 2 / 5).saturating_sub(1)].max(1e-20),
            extended[(extended.len() * 2 / 5).saturating_sub(1)].max(1e-20),
        ];
        let threshold = if settings.depth <= 2 { 2.1 } else { 1.3 };
        let mut candidates = Vec::new();
        for (bin, best) in peaks {
            for k in 0..2 {
                if k == 1 && best[1].1 == best[0].1 {
                    continue;
                }
                let score = best[k].0 / base[k];
                if score >= threshold {
                    candidates.push(Candidate {
                        frequency: bin as f32 * 3.125,
                        start: 0.5 + (best[k].1 as f32 - 0.5) * 0.04,
                        score,
                    });
                }
            }
        }
        candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
        candidates.truncate(1000);
        let mut filtered: Vec<Candidate> = Vec::new();
        for c in candidates {
            if !filtered.iter().any(|p| {
                (p.frequency - c.frequency).abs() < 4.0 && (p.start - c.start).abs() < 0.04
            }) {
                filtered.push(c)
            }
        }
        if let Some(priority) = settings.priority_hz {
            filtered.sort_by_key(|c| {
                if (c.frequency - priority).abs() <= settings.priority_tolerance_hz {
                    0
                } else {
                    1
                }
            });
        }
        filtered.truncate(1000);
        let baseline = self.baseline(pcm, low, high, cancel)?;
        Ok((filtered, baseline))
    }
    fn baseline(
        &mut self,
        pcm: &[f32],
        low: usize,
        high: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<f32>, String> {
        let mut window: Vec<f32> = (0..3840)
            .map(|i| {
                let p = 2.0 * PI * i as f32 / 3840.0;
                0.3635819 - 0.4891775 * p.cos() + 0.1365995 * (2.0 * p).cos()
                    - 0.0106411 * (3.0 * p).cos()
            })
            .collect();
        let sum = window.iter().sum::<f32>();
        for w in &mut window {
            *w *= 3840.0 / 300.0 / sum
        }
        let mut avg = vec![0.0f32; 1921];
        let mut input = vec![0.0; 3840];
        let mut output = vec![ZERO; 1921];
        for j in 0..92 {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            for i in 0..3840 {
                input[i] = pcm[j * 1920 + i] * window[i]
            }
            self.coarse
                .transform(&mut input, &mut output)
                .map_err(|e| e.to_string())?;
            for i in 0..1921 {
                avg[i] += output[i].norm_sqr();
            }
        }
        let low = low.clamp(32, 1540);
        let high = high.clamp(low + 16, 1571);
        let len = (high.saturating_sub(low) + 1) / 10;
        for x in &mut avg {
            *x = 10.0 * x.max(1e-30).log10()
        }
        let mut matrix = [[0.0f64; 6]; 5];
        if len > 0 {
            for segment in 0..10 {
                let begin = low + segment * len;
                let mut sorted = avg[begin..begin + len].to_vec();
                sorted.sort_by(f32::total_cmp);
                let cutoff = sorted[(len / 10).saturating_sub(1)];
                for (i, &value) in avg.iter().enumerate().skip(begin).take(len) {
                    if value > cutoff {
                        continue;
                    }
                    let x = (i as f64 - (low + high) as f64 / 2.0) / (high - low).max(1) as f64;
                    let mut powers = [1.0f64; 9];
                    for k in 1..9 {
                        powers[k] = powers[k - 1] * x
                    }
                    for row in 0..5 {
                        for col in 0..5 {
                            matrix[row][col] += powers[row + col]
                        }
                        matrix[row][5] += powers[row] * value as f64
                    }
                }
            }
        }
        let mut valid = len > 0;
        for pivot in 0..5 {
            let row = (pivot..5)
                .max_by(|&a, &b| matrix[a][pivot].abs().total_cmp(&matrix[b][pivot].abs()))
                .unwrap();
            matrix.swap(pivot, row);
            let d = matrix[pivot][pivot];
            if d.abs() < 1e-12 {
                valid = false;
                break;
            }
            for value in &mut matrix[pivot][pivot..] {
                *value /= d;
            }
            for row in 0..5 {
                if row == pivot {
                    continue;
                }
                let d = matrix[row][pivot];
                let pivot_values = matrix[pivot];
                for (col, value) in matrix[row].iter_mut().enumerate().skip(pivot) {
                    *value -= d * pivot_values[col];
                }
            }
        }
        let mut baseline = vec![0.0; 1921];
        for i in 0..1921 {
            let x = (i as f64 - (low + high) as f64 / 2.0) / (high - low).max(1) as f64;
            let y = if valid {
                (0..5).rev().fold(0.0, |y, k| y * x + matrix[k][5]) as f32 + 0.65
            } else {
                avg[i]
            };
            baseline[i] = 10.0f32.powf((y - 40.0) * 0.1);
        }
        Ok(baseline)
    }
    #[allow(clippy::too_many_arguments)]
    fn candidate(
        &mut self,
        c: Candidate,
        settings: &DecodeSettings,
        squared: bool,
        stage: AcquisitionStage,
        baseline: &[f32],
        messages: &mut MessageDecoder,
        cancel: &AtomicBool,
    ) -> Result<Option<(DecodedSignal, [u8; 79], f32)>, String> {
        let mut data = vec![ZERO; 3200];
        self.down
            .extract(c.frequency, &mut data)
            .map_err(|e| e.to_string())?;
        let initial = (c.start * 200.0).round() as i32;
        let mut best = initial;
        let mut maximum = 0.0;
        for start in initial - 10..=initial + 10 {
            let score = sync(&data, start, 0.0);
            if score > maximum {
                maximum = score;
                best = start
            }
        }
        maximum = 0.0;
        let mut delta = 0.0;
        for f in -5..=5 {
            let score = sync(&data, best, f as f32 * 0.5);
            if score > maximum {
                maximum = score;
                delta = f as f32 * 0.5
            }
        }
        let frequency = c.frequency + delta;
        // Acquisition bins may lie outside the requested range; judge the refined frequency.
        if !(settings.low_hz..=settings.high_hz).contains(&frequency) {
            return Ok(None);
        }
        self.down
            .extract(frequency, &mut data)
            .map_err(|e| e.to_string())?;
        maximum = 0.0;
        let mut fine = best;
        for start in best - 4..=best + 4 {
            let score = sync(&data, start, 0.0);
            if score > maximum {
                maximum = score;
                fine = start
            }
        }
        let start = (fine - 1) as f32 / 200.0;
        let mut symbols = [[ZERO; 8]; 79];
        let mut work = [ZERO; 32];
        for (k, sym) in symbols.iter_mut().enumerate() {
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
        let mut nsync = 0;
        for block in [0, 36, 72] {
            for (i, &tone) in COSTAS.iter().enumerate() {
                let peak = (0..8)
                    .max_by(|&a, &b| {
                        symbols[block + i][a]
                            .norm_sqr()
                            .total_cmp(&symbols[block + i][b].norm_sqr())
                    })
                    .unwrap();
                if tone == peak {
                    nsync += 1
                }
            }
        }
        if nsync
            <= if settings.depth <= 2 {
                8
            } else if squared {
                7
            } else {
                6
            }
        {
            return Ok(None);
        }
        // Subtraction can expose a signal that never had usable ordinary
        // metrics on the first pass. At depth 3, retry those metrics after the
        // squared metrics fail, reusing the same refined symbol spectra.
        let metric_modes =
            std::iter::once(squared).chain((squared && settings.depth == 3).then_some(false));
        let unassisted = metric_modes
            .flat_map(|squared| metrics(&symbols, squared))
            .map(|llr| (llr, [false; 174], None));
        // Native AP follows unassisted trials and runs only with final audio.
        // Each hypothesis uses the single- and three-symbol metric variants.
        let assisted = std::iter::once_with(|| {
            if stage == AcquisitionStage::Early {
                Vec::new()
            } else {
                crate::assistance::hypotheses(settings, crate::engine::Mode::Ft8, frequency)
            }
        })
        .flatten()
        .flat_map(|hint| {
            let family = metrics(&symbols, squared);
            [0, 2].map(|i| {
                let magnitude = family[i].iter().map(|v| v.abs()).fold(0.0_f32, f32::max) * 1.1;
                (
                    hint.apply(&family[i], magnitude),
                    hint.mask,
                    Some(hint.ap_type),
                )
            })
        });
        for (llr, mask, ap_type) in unassisted.chain(assisted) {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let decoded = if settings.depth == 1 {
                fec::decode_ap(&llr, &mask, 30)
                    .map_err(|e| e.to_string())?
                    .map(|d| {
                        let mut diagnostics = DecodeDiagnostics::bp(&d);
                        if ap_type.is_some() {
                            let distance: f32 = d
                                .codeword
                                .iter()
                                .zip(llr)
                                .filter(|(b, v)| **b != (*v >= 0.0))
                                .map(|(_, v)| v.abs())
                                .sum();
                            diagnostics.confidence =
                                Some(1.0 - (d.hard_errors as f32 + distance) / 60.0);
                        }
                        (d, diagnostics)
                    })
            } else {
                fec::decode_hybrid_ap(&llr, &mask, 2, 2)
                    .map_err(|e| e.to_string())?
                    .map(|d| {
                        let mut diagnostics = DecodeDiagnostics::hybrid(&d);
                        if ap_type.is_some() {
                            diagnostics.confidence =
                                Some(1.0 - (d.decoded.hard_errors as f32 + d.distance) / 60.0);
                        }
                        (d.decoded, diagnostics)
                    })
            };
            let Some((decoded, mut diagnostics)) = decoded else {
                continue;
            };
            diagnostics.ap_type = ap_type;
            diagnostics.questionable = diagnostics.confidence.is_some_and(|q| q < 0.17);
            if decoded.hard_errors > 36 || decoded.codeword.iter().all(|&b| !b) {
                continue;
            }
            let bits = &decoded.message;
            let n3 = bits[71..74].iter().fold(0, |v, &b| v * 2 + u8::from(b));
            let i3 = bits[74..77].iter().fold(0, |v, &b| v * 2 + u8::from(b));
            if i3 > 5 || (i3 == 0 && (n3 > 6 || n3 == 2)) {
                continue;
            }
            let Ok(message) = messages.unpack(bits) else {
                continue;
            };
            if settings.ap.activity == crate::assistance::Activity::Normal
                && (1..=3).contains(&i3)
                && (message.text.contains("/R") || message.text.starts_with("TU; "))
            {
                continue;
            }
            let tones = symbols::ft8(&decoded.codeword);
            let power = tones
                .iter()
                .enumerate()
                .map(|(k, &t)| symbols[k][t as usize].norm_sqr() * 1e6)
                .sum::<f32>();
            let base = baseline[(c.frequency / 3.125).round() as usize];
            let ratio = (power / base / 3e6 - 1.0).max(0.001);
            let snr = 10.0 * ratio.log10() - 27.0;
            if nsync <= 10 && snr < -25.0 {
                continue;
            }
            return Ok(Some((
                DecodedSignal {
                    source_bits: *bits,
                    message,
                    frequency_hz: frequency,
                    snr_db: snr.max(-25.0).round() as i32,
                    dt_seconds: start - 0.5,
                    assisted: ap_type.is_some(),
                    diagnostics: Some(diagnostics),
                },
                tones,
                start,
            )));
        }
        Ok(None)
    }
}
fn cancelled(cancel: &AtomicBool, stats: &mut DecodeStats) -> bool {
    if cancel.load(Ordering::Relaxed) {
        stats.cancelled = true;
        true
    } else {
        false
    }
}
pub(super) fn sync(data: &[C], start: i32, offset: f32) -> f32 {
    let mut score = 0.0;
    for (i, &tone) in COSTAS.iter().enumerate() {
        let step = C::from_polar(1.0, -2.0 * PI * (tone as f32 / 32.0 + offset / 200.0));
        for block in [0, 36, 72] {
            let begin = start + (block + i) as i32 * 32;
            if begin < 0 || begin + 31 >= 2812 {
                continue;
            }
            let mut sum = ZERO;
            let mut phase = C::new(1.0, 0.0);
            for &sample in &data[begin as usize..begin as usize + 32] {
                sum += sample * phase;
                phase *= step
            }
            score += sum.norm_sqr();
        }
    }
    score
}
pub(super) fn metrics(symbols: &[[C; 8]; 79], squared: bool) -> [[f32; 174]; 5] {
    let mut metrics = [[0.0f32; 174]; 5];
    for count in 1..=3 {
        let variants = 1usize << (3 * count);
        for half in 0..2 {
            for k in (0..29).step_by(count) {
                let symbol = 7 + 36 * half + k;
                let bit = 87 * half + 3 * k;
                let mut values = [0.0f32; 512];
                for (index, value) in values.iter_mut().enumerate().take(variants) {
                    let mut sum = ZERO;
                    for n in 0..count {
                        sum += symbols[symbol + n][GRAY[(index >> (3 * (count - 1 - n))) & 7]]
                    }
                    *value = if squared { sum.norm_sqr() } else { sum.norm() };
                }
                for b in 0..3 * count {
                    if bit + b >= 174 {
                        continue;
                    }
                    let mask = 1 << (3 * count - 1 - b);
                    let mut maxima = [0.0f32; 2];
                    for (i, &v) in values.iter().enumerate().take(variants) {
                        let set = usize::from(i & mask != 0);
                        maxima[set] = maxima[set].max(v)
                    }
                    let difference = maxima[1] - maxima[0];
                    metrics[count - 1][bit + b] = difference;
                    if count == 1 {
                        metrics[3][bit + b] = difference / maxima[0].max(maxima[1]).max(1e-30)
                    }
                }
            }
        }
    }
    let (base, best) = metrics.split_at_mut(4);
    for (i, value) in best[0].iter_mut().enumerate() {
        *value = (0..3)
            .map(|k| base[k][i])
            .max_by(|a, b| a.abs().total_cmp(&b.abs()))
            .unwrap()
    }
    for metric in &mut metrics {
        let mean = metric.iter().sum::<f32>() / 174.0;
        let mean2 = metric.iter().map(|x| x * x).sum::<f32>() / 174.0;
        let variance = mean2 - mean * mean;
        let scale = 2.83
            / if variance > 0.0 {
                variance.sqrt()
            } else {
                mean2.sqrt()
            }
            .max(1e-30);
        for x in metric {
            *x *= scale
        }
    }
    metrics
}
// Abramowitz-Stegun erf approximation (maximum absolute error < 1.5e-7).
fn erf(x: f32) -> f32 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let y = 1.0
        - (((((1.0614054 * t - 1.4531521) * t) + 1.4214138) * t - 0.28449672) * t + 0.2548296)
            * t
            * (-x * x).exp();
    y.copysign(x)
}
/// Pulse and phase table are invariant; the waveform depends only on tones and
/// frequency, so all timing-refinement trials share one generated reference.
struct Waveform {
    pulse: Vec<f32>,
    phase_table: Vec<C>,
    increments: Vec<f32>,
    samples: Vec<C>,
}
impl Waveform {
    fn new() -> Self {
        let pulse = (1..=5760)
            .map(|i| {
                let t = (i as f32 - 2880.0) / 1920.0;
                let c = PI * (2.0 / 2.0f32.ln()).sqrt() * 2.0;
                0.5 * (erf(c * (t + 0.5)) - erf(c * (t - 0.5)))
            })
            .collect();
        // Include the endpoint in case the f32 index calculation rounds up.
        let phase_table = (0..=65536)
            .map(|i| C::from_polar(1.0, i as f32 * (2.0 * PI) / 65536.0))
            .collect();
        Self {
            pulse,
            phase_table,
            increments: vec![0.0; 81 * 1920],
            samples: Vec::with_capacity(151680),
        }
    }
    fn generate(&mut self, tones: &[u8; 79], frequency: f32) {
        let increments = &mut self.increments;
        let pulse = &self.pulse;
        increments.fill(0.0);
        let scale = 2.0 * PI / 1920.0;
        for (s, &tone) in tones.iter().enumerate() {
            for (i, &p) in pulse.iter().enumerate() {
                increments[s * 1920 + i] += scale * p * tone as f32
            }
        }
        for i in 0..3840 {
            increments[i] += scale * tones[0] as f32 * pulse[1920 + i];
            increments[79 * 1920 + i] += scale * tones[78] as f32 * pulse[i]
        }
        for step in increments.iter_mut() {
            *step += 2.0 * PI * frequency / 12000.0;
        }
        let mut phase = 0.0f32;
        let wave = &mut self.samples;
        wave.clear();
        for &step in &increments[1920..80 * 1920] {
            let table_index = (phase * 65536.0 / (2.0 * PI)) as usize;
            wave.push(self.phase_table[table_index]);
            phase = (phase + step) % (2.0 * PI)
        }
        for i in 0..240 {
            wave[i] *= 0.5 * (1.0 - (PI * i as f32 / 240.0).cos());
            wave[151440 + i] *= 0.5 * (1.0 + (PI * i as f32 / 240.0).cos())
        }
    }
}

struct SubtractionCore {
    forward: ComplexFft,
    inverse: ComplexFft,
    filter: Vec<C>,
    correction: Vec<f32>,
    amplitudes: Vec<C>,
    reference: Waveform,
}
impl SubtractionCore {
    fn new() -> Self {
        let mut forward = ComplexFft::new(180000, Direction::Forward).unwrap();
        let inverse = ComplexFft::new(180000, Direction::Inverse).unwrap();
        let mut filter = vec![ZERO; 180000];
        let sum = (0..=4000)
            .map(|i| (PI * (i as f32 - 2000.0) / 4000.0).cos().powi(2))
            .sum::<f32>();
        for i in 0..=4000 {
            filter[(i + 180000 - 2000) % 180000] =
                C::new((PI * (i as f32 - 2000.0) / 4000.0).cos().powi(2) / sum, 0.0)
        }
        forward.transform(&mut filter).unwrap();
        for value in &mut filter {
            *value /= 180000.0;
        }
        let mut tail = 0.0;
        let mut correction = vec![1.0; 2001];
        for i in (0..=2000).rev() {
            tail += (PI * i as f32 / 4000.0).cos().powi(2);
            correction[i] = 1.0 / (1.0 - tail / sum)
        }
        Self {
            forward,
            inverse,
            filter,
            correction,
            amplitudes: vec![ZERO; 180000],
            reference: Waveform::new(),
        }
    }
    fn subtract_at(
        &mut self,
        pcm: &mut [f32],
        offset: i32,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let reference = &self.reference.samples;
        let amplitudes = &mut self.amplitudes;
        amplitudes.fill(ZERO);
        for (i, &r) in reference.iter().enumerate() {
            let j = offset + i as i32;
            if (0..180000).contains(&j) {
                amplitudes[i] = pcm[j as usize] * r.conj();
            }
        }
        self.forward
            .transform(amplitudes)
            .map_err(|e| e.to_string())?;
        for (a, &f) in amplitudes.iter_mut().zip(&self.filter) {
            *a *= f
        }
        self.inverse
            .transform(amplitudes)
            .map_err(|e| e.to_string())?;
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        for (i, &r) in reference.iter().enumerate() {
            let j = offset + i as i32;
            if !(0..180000).contains(&j) {
                continue;
            }
            let edge = i.min(reference.len() - 1 - i);
            let gain = if edge <= 2000 {
                self.correction[edge]
            } else {
                1.0
            };
            pcm[j as usize] -= 2.0 * (amplitudes[i] * r).re * gain;
        }
        Ok(())
    }
}

struct Refinement {
    fft: RealForward,
    residual: Vec<f32>,
    input: Vec<f32>,
    spectrum: Vec<C>,
}
impl Refinement {
    fn new() -> Self {
        Self {
            fft: RealForward::new(180000).unwrap(),
            residual: vec![0.0; 180000],
            input: vec![0.0; 180000],
            spectrum: vec![ZERO; 90001],
        }
    }
}

/// Owned by one decoder and allocated only after a signal needs subtraction.
/// Reuse is bounded by the fixed FT8 window sizes, including refinement scratch.
struct Subtractor {
    core: SubtractionCore,
    refinement: Option<Refinement>,
}
impl Subtractor {
    fn new() -> Self {
        Self {
            core: SubtractionCore::new(),
            refinement: None,
        }
    }
    fn subtract(
        &mut self,
        pcm: &mut [f32],
        tones: &[u8; 79],
        frequency: f32,
        start: f32,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.core.reference.generate(tones, frequency);
        // Fortran nstart is one-based and assigned by truncation, not rounding.
        self.core
            .subtract_at(pcm, (start * 12000.0 + 1.0) as i32 - 1, cancel)
    }
    // Native subtractft8 timing refinement: minimize remaining in-band power at
    // +/-90 samples and interpolate the minimum before reconstructing the signal.
    fn subtract_refined(
        &mut self,
        pcm: &mut [f32],
        tones: &[u8; 79],
        frequency: f32,
        start: f32,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.core.reference.generate(tones, frequency);
        let work = self.refinement.get_or_insert_with(Refinement::new);
        let mut powers = [0.0f32; 3];
        let base_offset = (start * 12000.0 + 1.0) as i32 - 1;
        for (slot, shift) in [-90i32, 0, 90].into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let offset = base_offset + shift;
            work.residual.copy_from_slice(pcm);
            self.core.subtract_at(&mut work.residual, offset, cancel)?;
            work.input.fill(0.0);
            for (i, value) in work.input.iter_mut().enumerate().take(151680) {
                let j = offset + i as i32;
                if (0..180000).contains(&j) {
                    *value = work.residual[j as usize]
                }
            }
            work.fft
                .transform(&mut work.input, &mut work.spectrum)
                .map_err(|e| e.to_string())?;
            let low = ((frequency - 1.5 * 6.25) * 15.0).max(0.0) as usize;
            let high = ((frequency + 8.5 * 6.25) * 15.0).min(90000.0) as usize;
            powers[slot] = work.spectrum[low..=high].iter().map(|z| z.norm_sqr()).sum();
        }
        let curvature = powers[2] + powers[0] - 2.0 * powers[1];
        let offset = -(powers[2] - powers[0]) / (2.0 * curvature);
        if !offset.is_finite() || offset.abs() > 1.0 {
            return Ok(());
        }
        self.core
            .subtract_at(pcm, base_offset + (90.0 * offset).round() as i32, cancel)
    }
}

#[cfg(test)]
fn waveform(tones: &[u8; 79], frequency: f32) -> Vec<C> {
    let mut wave = Waveform::new();
    wave.generate(tones, frequency);
    wave.samples
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reused_subtraction_clears_previous_waveform_and_trial_buffers() {
        let mut tones =
            b"3140652032247523504061147005134325373140652464557561564770300376175462233140652"
                .map(|b| b - b'0');
        let cancel = AtomicBool::new(false);
        let mut reused = Subtractor::new();
        // Alternate clipped leading/trailing signals, amplitudes, and depth.
        // Any stale waveform, FFT padding, or refinement residual changes output.
        for (frequency, offset, amplitude, refined) in [
            (1501.5, 6000, 100.0, true),
            (725.25, -1200, 17.0, false),
            (2200.0, 36000, 43.0, true),
        ] {
            tones[7] = (tones[7] + 1) % 8;
            tones[45] = (tones[45] + 3) % 8;
            let mut input = vec![0.0; 180000];
            for (i, value) in waveform(&tones, frequency).iter().enumerate() {
                let index = i as i32 + offset;
                if (0..180000).contains(&index) {
                    input[index as usize] = value.im * amplitude;
                }
            }
            let mut expected = input.clone();
            let mut actual = input.clone();
            let start = offset as f32 / 12000.0;
            let mut fresh = Subtractor::new();
            if refined {
                fresh
                    .subtract_refined(&mut expected, &tones, frequency, start, &cancel)
                    .unwrap();
                reused
                    .subtract_refined(&mut actual, &tones, frequency, start, &cancel)
                    .unwrap();
            } else {
                fresh
                    .subtract(&mut expected, &tones, frequency, start, &cancel)
                    .unwrap();
                reused
                    .subtract(&mut actual, &tones, frequency, start, &cancel)
                    .unwrap();
            }
            assert!(actual.iter().zip(&expected).all(|(a, b)| a == b));
            let cancelled = AtomicBool::new(true);
            reused
                .subtract_refined(&mut actual, &tones, frequency, start, &cancelled)
                .unwrap();
            assert!(actual.iter().zip(&expected).all(|(a, b)| a == b));
        }
    }

    #[test]
    fn synthesis_matches_independent_native_waveform() {
        let tones: [u8; 79] =
            b"3140652032247523504061147005134325373140652464557561564770300376175462233140652"
                .map(|b| b - b'0');
        let wave = waveform(&tones, 1501.5);
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/ft8-waveform-samples.json"
        ))
        .unwrap();
        for point in oracle["samples"].as_array().unwrap() {
            let i = point[0].as_u64().unwrap() as usize;
            let expected = point[1].as_f64().unwrap() as f32 / 32767.0;
            assert!(
                (wave[i].im - expected).abs() < 0.0005,
                "sample {i}: {} vs {expected}",
                wave[i].im
            );
        }
    }
    #[test]
    fn cancellation_removes_reconstructed_signal() {
        let tones: [u8; 79] =
            b"3140652032247523504061147005134325373140652464557561564770300376175462233140652"
                .map(|b| b - b'0');
        let wave = waveform(&tones, 1501.5);
        let mut pcm = vec![0.0; 180000];
        for (i, w) in wave.iter().enumerate() {
            pcm[6000 + i] = w.im * 100.0;
        }
        let before = pcm.iter().map(|x| x * x).sum::<f32>();
        Subtractor::new()
            .subtract(&mut pcm, &tones, 1501.5, 0.5, &AtomicBool::new(false))
            .unwrap();
        let after = pcm.iter().map(|x| x * x).sum::<f32>();
        assert!(
            after / before < 0.001,
            "remaining power fraction {}",
            after / before
        );
    }
    #[test]
    fn mixture_weak_signal_is_not_decodable_without_subtraction() {
        let raw = include_bytes!("../../tests/fixtures/ft8-mixture.wav");
        let pcm: Vec<f32> = raw[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b) as f32)
            .collect();
        let mut decoder = Ft8Decoder::new();
        let settings = DecodeSettings::default();
        let cancel = AtomicBool::new(false);
        let (candidates, baseline) = decoder.acquire(&pcm, &settings, &cancel).unwrap();
        decoder.down.prepare(&pcm).unwrap();
        let mut history = MessageDecoder::default();
        let mut strong = false;
        for squared in [false, true] {
            for &candidate in &candidates {
                if let Some((signal, _, _)) = decoder
                    .candidate(
                        candidate,
                        &settings,
                        squared,
                        AcquisitionStage::Full,
                        &baseline,
                        &mut history,
                        &cancel,
                    )
                    .unwrap()
                {
                    assert_ne!(signal.message.text, "CQ K1ABC FN42");
                    strong |= signal.message.text == "K1ABC W9XYZ EN37";
                }
            }
        }
        assert!(strong);
    }
    #[test]
    fn identical_messages_at_distinct_frequencies_are_retained() {
        let tones: [u8; 79] =
            b"3140652000000001005476704606021533433140652736011047517007334745455133543140652"
                .map(|b| b - b'0');
        let mut pcm = vec![0.0; 180000];
        for frequency in [1000.0, 1600.0] {
            for (i, w) in waveform(&tones, frequency).iter().enumerate() {
                pcm[6000 + i] += 20.0 * w.im;
            }
        }
        let mut found = Vec::new();
        Ft8Decoder::new()
            .decode(
                &pcm,
                &DecodeSettings::default(),
                &mut MessageDecoder::default(),
                &AtomicBool::new(false),
                &mut |d| found.push(d),
            )
            .unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|d| d.message.text == "CQ K1ABC FN42"));
    }
    #[test]
    fn nearby_weak_signal_requires_timing_refined_subtraction() {
        let strong: [u8; 79] =
            b"3140652032247523504061147005134325373140652464557561564770300376175462233140652"
                .map(|b| b - b'0');
        let weak: [u8; 79] =
            b"3140652000000001005476704606021533433140652736011047517007334745455133543140652"
                .map(|b| b - b'0');
        let raw = include_bytes!("../../tests/fixtures/ft8-noise.wav");
        let mut pcm: Vec<f32> = raw[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b) as f32)
            .collect();
        for (tones, freq, start, snr) in [
            (&strong, 1501.375, 0.755, 16.0),
            (&weak, 1536.25, 0.695, -17.0),
        ] {
            let amplitude = 100.0 * (2.0f32 * 2500.0 / 6000.0).sqrt() * 10.0f32.powf(snr / 20.0);
            for (i, sample) in waveform(tones, freq).iter().enumerate() {
                pcm[(start * 12000.0) as usize + i] += amplitude * sample.im;
            }
        }
        let cancel = AtomicBool::new(false);
        let mut coarse = pcm.clone();
        Subtractor::new()
            .subtract(&mut coarse, &strong, 1501.375, 0.750, &cancel)
            .unwrap();
        Subtractor::new()
            .subtract_refined(&mut pcm, &strong, 1501.375, 0.750, &cancel)
            .unwrap();
        let has_weak = |audio: &[f32]| {
            let mut decoder = Ft8Decoder::new();
            let settings = DecodeSettings::default();
            decoder.down.prepare_residual(audio).unwrap();
            let baseline = decoder.baseline(audio, 64, 960, &cancel).unwrap();
            let mut messages = MessageDecoder::default();
            [false, true].into_iter().any(|squared| {
                decoder
                    .candidate(
                        Candidate {
                            frequency: 1536.25,
                            start: 0.695,
                            score: 10.0,
                        },
                        &settings,
                        squared,
                        AcquisitionStage::Full,
                        &baseline,
                        &mut messages,
                        &cancel,
                    )
                    .unwrap()
                    .is_some_and(|(signal, _, _)| signal.message.text == "CQ K1ABC FN42")
            })
        };
        assert!(
            !has_weak(&coarse),
            "coarse subtraction should leave masking interference"
        );
        assert!(
            has_weak(&pcm),
            "refinement should reveal the adjacent weak signal"
        );
    }
}
