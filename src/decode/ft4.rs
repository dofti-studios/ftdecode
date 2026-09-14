// SPDX-License-Identifier: GPL-3.0-or-later
//! FT4 receiver with native AP hypothesis decoding, ported from WSJT-X lib/ft4_decode.f90 and
//! lib/ft4/{getcandidates4,sync4d,get_ft4_bitmetrics,subtractft4,gen_ft4wave}.f90
//! at ccdfaf3c1c109010d15399674ce278167cfde848. See NOTICE.md.
use crate::{
    assistance,
    downsample::Ft4Downsampler,
    engine::{DecodeDiagnostics, DecodeSettings, DecodeStats, DecodedSignal, Mode},
    fec,
    fft::{Complex32 as C, ComplexFft, Direction, RealForward},
    message::MessageDecoder,
    symbols,
};
use std::{
    f32::consts::{PI, TAU},
    sync::atomic::{AtomicBool, Ordering},
};
const N: usize = 72576;
const FS: f32 = 12000.0 / 18.0;
const SYNC: [[usize; 4]; 4] = [[0, 1, 3, 2], [1, 0, 2, 3], [2, 3, 1, 0], [3, 2, 0, 1]];
const GRAY: [usize; 4] = [0, 1, 3, 2];

pub struct Ft4Decoder {
    pub(crate) filtered_input: bool,
    down: Ft4Downsampler,
    coarse: RealForward,
    symbol: ComplexFft,
    forward: ComplexFft,
    inverse: ComplexFft,
    window: Vec<f32>,
    sync: Vec<[[C; 64]; 4]>,
    filter: Vec<C>,
}
impl Default for Ft4Decoder {
    fn default() -> Self {
        Self::new()
    }
}
impl Ft4Decoder {
    pub fn new() -> Self {
        let sync = (-16..=16)
            .map(|df| {
                std::array::from_fn(|b| {
                    std::array::from_fn(|k| {
                        let phase = TAU
                            * (SYNC[b][k / 16] as f32 * (k % 16) as f32 / 16.0
                                + df as f32 * (k + 1) as f32 / (FS / 2.0));
                        C::from_polar(1.0, phase)
                    })
                })
            })
            .collect();
        let window = (0..2304)
            .map(|i| {
                let p = TAU * i as f32 / 2304.0;
                0.3635819 - 0.4891775 * p.cos() + 0.1365995 * (2.0 * p).cos()
                    - 0.0106411 * (3.0 * p).cos()
            })
            .collect();
        let mut forward = ComplexFft::new(N, Direction::Forward).unwrap();
        let mut filter = vec![C::default(); N];
        let sum: f32 = (-700..=700)
            .map(|j| (PI * j as f32 / 1400.0).cos().powi(2))
            .sum();
        // Native cshift(...,701) places the centre at index N-1.
        for j in 0..=1400 {
            filter[(j + N - 701) % N].re =
                (PI * (j as f32 - 700.0) / 1400.0).cos().powi(2) / (sum * N as f32);
        }
        forward.transform(&mut filter).unwrap();
        Self {
            filtered_input: false,
            down: Ft4Downsampler::new(),
            coarse: RealForward::new(2304).unwrap(),
            symbol: ComplexFft::new(32, Direction::Forward).unwrap(),
            forward,
            inverse: ComplexFft::new(N, Direction::Inverse).unwrap(),
            window,
            sync,
            filter,
        }
    }
    pub fn decode(
        &mut self,
        pcm: &[f32],
        settings: &DecodeSettings,
        messages: &mut MessageDecoder,
        cancel: &AtomicBool,
        on_decode: &mut dyn FnMut(DecodedSignal),
    ) -> Result<DecodeStats, String> {
        if pcm.len() != N
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
            return Err(format!(
                "FT4 requires {N} finite PCM samples in [-32768,32768]"
            ));
        }
        settings.validate()?;
        let mut residual = pcm.to_vec();
        let mut stats = DecodeStats::default();
        let mut accepted: Vec<([bool; 77], f32, f32)> = Vec::new();
        for pass in 0..if settings.depth == 1 { 1 } else { 3 } {
            if cancel.load(Ordering::Relaxed) {
                stats.cancelled = true;
                break;
            }
            stats.passes += 1;
            let candidates = self.candidates(&residual, settings, cancel)?;
            if cancel.load(Ordering::Relaxed) {
                stats.cancelled = true;
                break;
            }
            self.down
                .prepare_residual(&residual)
                .map_err(|e| e.to_string())?;
            let mut count = 0;
            for (f0, peak) in candidates {
                if cancel.load(Ordering::Relaxed) {
                    stats.cancelled = true;
                    break;
                }
                stats.candidates += 1;
                let mut cd = vec![C::default(); 4032];
                self.down.extract(f0, &mut cd).map_err(|e| e.to_string())?;
                normalize(&mut cd, 4032.0);
                let mut first_sync = 0.0;
                for segment in 0..3 {
                    if cancel.load(Ordering::Relaxed) {
                        stats.cancelled = true;
                        break;
                    }
                    let (lo, hi) = match segment {
                        0 => (108, 560),
                        1 => (560, 1012),
                        _ => (-344, 108),
                    };
                    let (mut best, mut frequency, mut power) = (0, 0, -1.0);
                    for df in (-12..=12).step_by(3) {
                        if !(0.0..=5000.0).contains(&(f0 + df as f32)) {
                            continue;
                        }
                        for start in (lo..=hi).step_by(4) {
                            let p = self.sync_power(&cd, start, df);
                            if p > power {
                                (best, frequency, power) = (start, df, p);
                            }
                        }
                    }
                    let (b, f) = (best, frequency);
                    power = -1.0;
                    for df in f - 4..=f + 4 {
                        if !(0.0..=5000.0).contains(&(f0 + df as f32)) {
                            continue;
                        }
                        for start in b - 5..=b + 5 {
                            let p = self.sync_power(&cd, start, df);
                            if p > power {
                                (best, frequency, power) = (start, df, p);
                            }
                        }
                    }
                    if segment == 0 {
                        first_sync = power;
                    }
                    if power < 1.2 || (segment > 0 && power < first_sync) {
                        continue;
                    }
                    let f1 = f0 + frequency as f32;
                    if !(0.0..=5000.0).contains(&f1)
                        || f1 < settings.low_hz
                        || f1 > settings.high_hz
                    {
                        continue;
                    }
                    let mut aligned = vec![C::default(); 4032];
                    self.down
                        .extract(f1, &mut aligned)
                        .map_err(|e| e.to_string())?;
                    normalize(&mut aligned, 3296.0);
                    let Some(metrics) = self.metrics(&aligned, best)? else {
                        continue;
                    };
                    let mut success = false;
                    let llrs: [[f32; 174]; 3] = std::array::from_fn(|m| {
                        std::array::from_fn(|i| metrics[m][8 + i + 8 * (i / 58)] * 2.83)
                    });
                    // Native FT4 uses llra to scale AP, and llrc as its channel
                    // input after all three unassisted metric variants fail.
                    let apmag = llrs[0].iter().map(|v| v.abs()).fold(0.0_f32, f32::max) * 1.1;
                    let hypotheses = assistance::hypotheses(settings, Mode::Ft4, f1);
                    for attempt in 0..3 + hypotheses.len() {
                        if cancel.load(Ordering::Relaxed) {
                            stats.cancelled = true;
                            break;
                        }
                        let hypothesis = attempt.checked_sub(3).map(|i| &hypotheses[i]);
                        let llr = hypothesis
                            .map_or_else(|| llrs[attempt.min(2)], |h| h.apply(&llrs[2], apmag));
                        let mask = hypothesis.map_or(&[false; 174], |h| &h.mask);
                        let decoded = if settings.depth == 3 {
                            let maxosd =
                                if settings.priority_hz.is_some_and(|f| (f - f1).abs() <= 50.0) {
                                    3
                                } else {
                                    2
                                };
                            fec::decode_hybrid_ap(&llr, mask, maxosd, 2)
                                .map_err(|e| e.to_string())?
                                .map(|d| {
                                    let mut diagnostics = DecodeDiagnostics::hybrid(&d);
                                    diagnostics.confidence = Some(
                                        1.0 - (d.decoded.hard_errors as f32 + d.distance) / 60.0,
                                    );
                                    (d.decoded, diagnostics)
                                })
                        } else {
                            fec::decode_ap(&llr, mask, 30)
                                .map_err(|e| e.to_string())?
                                .map(|d| {
                                    let mut diagnostics = DecodeDiagnostics::bp(&d);
                                    let distance: f32 = d
                                        .codeword
                                        .iter()
                                        .zip(&llr)
                                        .filter(|(bit, value)| **bit != (**value >= 0.0))
                                        .map(|(_, value)| value.abs())
                                        .sum();
                                    diagnostics.confidence =
                                        Some(1.0 - (d.hard_errors as f32 + distance) / 60.0);
                                    (d, diagnostics)
                                })
                        };
                        let Some((decoded, mut diagnostics)) = decoded else {
                            continue;
                        };
                        diagnostics.pass = pass + 1;
                        diagnostics.ap_type = hypothesis.map(|h| h.ap_type);
                        diagnostics.questionable =
                            diagnostics.confidence.is_some_and(|q| q <= 0.17);
                        if !decoded.message.iter().any(|b| *b) {
                            continue;
                        }
                        let payload = symbols::scramble_ft4(&decoded.message);
                        let Ok(message) = messages.unpack(&payload) else {
                            break;
                        };
                        let dt = best as f32 / FS - 0.5;
                        if settings.depth > 1 {
                            self.subtract(
                                &mut residual,
                                &symbols::ft4(&decoded.codeword),
                                f1,
                                best,
                            )?;
                        }
                        success = true;
                        if accepted.iter().any(|(bits, f, t)| {
                            *bits == payload && (f - f1).abs() < 6.0 && (t - dt).abs() < 0.12
                        }) {
                            break;
                        }
                        accepted.push((payload, f1, dt));
                        count += 1;
                        stats.decoded += 1;
                        let snr = if peak > 1.0 {
                            10.0 * (peak - 1.0).log10() - 14.8
                        } else {
                            -21.0
                        };
                        on_decode(DecodedSignal {
                            source_bits: payload,
                            message,
                            frequency_hz: f1,
                            snr_db: snr.max(-21.0).round() as i32,
                            dt_seconds: dt,
                            assisted: hypothesis.is_some(),
                            diagnostics: Some(diagnostics),
                        });
                        break;
                    }
                    if success {
                        break;
                    }
                }
            }
            if stats.cancelled || count == 0 || pass == 2 {
                break;
            }
        }
        stats.cancelled |= cancel.load(Ordering::Relaxed);
        Ok(stats)
    }
    fn sync_power(&self, cd: &[C], start: i32, df: i32) -> f32 {
        let references = &self.sync[(df + 16) as usize];
        let mut power = 0.0;
        for (b, reference) in references.iter().enumerate() {
            let offset = start + b as i32 * 1056;
            let mut sum = C::default();
            let mut count = 0;
            for (k, value) in reference.iter().enumerate() {
                let i = offset + 2 * k as i32;
                if (0..4032).contains(&i) {
                    sum += cd[i as usize] * value.conj();
                    count += 1;
                }
            }
            if count > 16 {
                power += sum.norm() / 64.0;
            }
        }
        power
    }
    fn metrics(&mut self, cd: &[C], start: i32) -> Result<Option<[[f32; 206]; 3]>, String> {
        let mut cs = [[C::default(); 4]; 103];
        for (k, row) in cs.iter_mut().enumerate() {
            let mut x = std::array::from_fn::<_, 32, _>(|i| {
                cd.get((start + k as i32 * 32 + i as i32) as usize)
                    .copied()
                    .unwrap_or_default()
            });
            self.symbol.transform(&mut x).map_err(|e| e.to_string())?;
            row.copy_from_slice(&x[..4]);
        }
        let mut nsync = 0;
        for b in 0..4 {
            for k in 0..4 {
                let row = cs[b * 33 + k];
                let strongest = (0..4)
                    .max_by(|a, b| row[*a].norm_sqr().total_cmp(&row[*b].norm_sqr()))
                    .unwrap();
                nsync += usize::from(strongest == SYNC[b][k]);
            }
        }
        if nsync < 8 {
            return Ok(None);
        }
        let mut metrics = [[0.0; 206]; 3];
        for (variant, nsym) in [1, 2, 4].into_iter().enumerate() {
            let nt = 1 << (2 * nsym);
            for k in (0..=103 - nsym).step_by(nsym) {
                let mut maxima = [[0.0_f32; 2]; 8];
                for sequence in 0..nt {
                    let mut z = C::default();
                    for j in 0..nsym {
                        let tone = GRAY[(sequence >> (2 * (nsym - 1 - j))) & 3];
                        z += cs[k + j][tone];
                    }
                    let magnitude = z.norm();
                    for (bit, max) in maxima.iter_mut().enumerate().take(2 * nsym) {
                        let value = (sequence >> (2 * nsym - 1 - bit)) & 1;
                        max[value] = max[value].max(magnitude);
                    }
                }
                for bit in 0..2 * nsym {
                    metrics[variant][2 * k + bit] = maxima[bit][1] - maxima[bit][0];
                }
            }
        }
        metrics[1][204] = metrics[0][204];
        metrics[1][205] = metrics[0][205];
        let tail: [f32; 4] = metrics[1][200..204].try_into().unwrap();
        metrics[2][200..204].copy_from_slice(&tail);
        metrics[2][204] = metrics[0][204];
        metrics[2][205] = metrics[0][205];
        // Native bit sync quality uses individual-bit maxima, not hard-tone bits.
        let mut quality = 0;
        for b in 0..4 {
            for k in 0..4 {
                for bit in 0..2 {
                    quality += usize::from(
                        (metrics[0][2 * (b * 33 + k) + bit] >= 0.0)
                            == ((GRAY[SYNC[b][k]] >> (1 - bit)) & 1 != 0),
                    );
                }
            }
        }
        if quality < 20 {
            return Ok(None);
        }
        for metric in &mut metrics {
            let mean = metric.iter().sum::<f32>() / 206.0;
            let square = metric.iter().map(|x| x * x).sum::<f32>() / 206.0;
            let variance = square - mean * mean;
            let sigma = if variance > 0.0 {
                variance.sqrt()
            } else {
                square.sqrt()
            };
            if sigma <= 0.0 {
                return Ok(None);
            }
            for v in metric {
                *v /= sigma;
            }
        }
        Ok(Some(metrics))
    }
    fn candidates(
        &mut self,
        pcm: &[f32],
        settings: &DecodeSettings,
        cancel: &AtomicBool,
    ) -> Result<Vec<(f32, f32)>, String> {
        let mut spectrum = vec![0.0_f32; 1153];
        let mut x = vec![0.0; 2304];
        let mut cx = vec![C::default(); 1153];
        for step in 0..122 {
            if cancel.load(Ordering::Relaxed) {
                return Ok(Vec::new());
            }
            for i in 0..2304 {
                x[i] = pcm[step * 576 + i] * self.window[i] / 300.0;
            }
            self.coarse
                .transform(&mut x, &mut cx)
                .map_err(|e| e.to_string())?;
            for i in 1..=1152 {
                spectrum[i] += cx[i].norm_sqr() / 122.0;
            }
        }
        let df = 12000.0 / 2304.0;
        // Acquisition measures the centre of all four tones. Expand the
        // lowest-tone limits by that offset, fine-search reach, and neighbour
        // bins. Apply the caller's exact bounds only after synchronization.
        let lo = (((settings.low_hz + 31.25 - 21.0) / df).floor() as usize).max(1);
        let hi = (((settings.high_hz + 31.25 + 21.0) / df).ceil() as usize).min(1151);
        // Preserve the reference baseline band while permitting narrow and
        // near-DC searches. At least 100 bins keep its polynomial fit stable.
        let fit_start = ((settings.low_hz / df) as usize).clamp(1, 1051);
        let fit_end = ((settings.high_hz / df) as usize).clamp(fit_start + 1, 1151);
        let missing = 100_usize.saturating_sub(fit_end - fit_start);
        let fitlo = fit_start.saturating_sub(missing / 2).clamp(1, 1051);
        let fithi = fit_end.max(fitlo + 100).min(1151);
        let baseline = baseline(&spectrum, fitlo, fithi);
        let mut smoothed = vec![0.0_f32; 1153];
        for i in lo - 1..=hi + 1 {
            let left = i.saturating_sub(7);
            let right = (i + 7).min(1152);
            smoothed[i] = spectrum[left..=right].iter().sum::<f32>()
                / ((right - left + 1) as f32 * baseline[i]);
        }
        let mut out = Vec::new();
        for i in lo..=hi {
            let p = smoothed[i];
            if p < 1.18 || !p.is_finite() || p < smoothed[i - 1] || p < smoothed[i + 1] {
                continue;
            }
            let den = smoothed[i - 1] - 2.0 * p + smoothed[i + 1];
            let delta = if den != 0.0 {
                0.5 * (smoothed[i - 1] - smoothed[i + 1]) / den
            } else {
                0.0
            };
            let f = (i as f32 + delta) * df - 31.25;
            if !(-31.25..=6000.0).contains(&f) {
                continue;
            }
            out.push((
                f.max(0.0),
                p - 0.25 * (smoothed[i - 1] - smoothed[i + 1]) * delta,
            ));
            if out.len() == 200 {
                break;
            }
        }
        // Near DC the one-sided spectrum can have its maximum at the
        // boundary rather than an interior peak. Let fine synchronization
        // test that boundary explicitly.
        if settings.low_hz <= 16.0 {
            out.push((0.0, smoothed[6]));
        }
        if let Some(priority) = settings.priority_hz {
            out.sort_by_key(|(f, _)| {
                usize::from((*f - priority).abs() > settings.priority_tolerance_hz)
            });
        }
        Ok(out)
    }
    fn subtract(
        &mut self,
        pcm: &mut [f32],
        tones: &[u8; 105],
        frequency: f32,
        start: i32,
    ) -> Result<(), String> {
        let reference = waveform(tones, frequency);
        let offset = start * 18 - 576;
        let mut amplitude = vec![C::default(); N];
        for (i, r) in reference.iter().enumerate() {
            if let Some(v) = pcm.get((offset + i as i32) as usize) {
                amplitude[i] = r.conj() * *v;
            }
        }
        self.forward
            .transform(&mut amplitude)
            .map_err(|e| e.to_string())?;
        for (a, w) in amplitude.iter_mut().zip(&self.filter) {
            *a *= *w;
        }
        self.inverse
            .transform(&mut amplitude)
            .map_err(|e| e.to_string())?;
        for (i, r) in reference.iter().enumerate() {
            if let Some(v) = pcm.get_mut((offset + i as i32) as usize) {
                *v -= 2.0 * (amplitude[i] * *r).re;
            }
        }
        Ok(())
    }
}
fn normalize(cd: &mut [C], denominator: f32) {
    let energy = cd.iter().map(|c| c.norm_sqr()).sum::<f32>() / denominator;
    if energy > 0.0 {
        let scale = energy.sqrt().recip();
        for z in cd {
            *z *= scale;
        }
    }
}
fn baseline(s: &[f32], lo: usize, hi: usize) -> Vec<f32> {
    let segment = (hi - lo + 1) / 10;
    let middle = (hi - lo).div_ceil(2);
    let mut normal = [[0.0_f64; 6]; 5];
    for n in 0..10 {
        let a = lo + n * segment;
        let b = a + segment;
        let mut values: Vec<f32> = s[a..b]
            .iter()
            .map(|x| 10.0 * x.max(1e-30).log10())
            .collect();
        values.sort_by(f32::total_cmp);
        let percentile = values[((segment as f32 * 0.1).round() as usize).saturating_sub(1)];
        for (j, power) in s.iter().enumerate().take(b).skip(a) {
            let y = 10.0 * power.max(1e-30).log10();
            if y > percentile {
                continue;
            }
            let x = (j as f64 - middle as f64) / 1000.0;
            let mut powers = [1.0; 9];
            for p in 1..9 {
                powers[p] = powers[p - 1] * x;
            }
            for row in 0..5 {
                for col in 0..5 {
                    normal[row][col] += powers[row + col];
                }
                normal[row][5] += powers[row] * y as f64;
            }
        }
    }
    for col in 0..5 {
        let pivot = (col..5)
            .max_by(|a, b| normal[*a][col].abs().total_cmp(&normal[*b][col].abs()))
            .unwrap();
        normal.swap(col, pivot);
        let divisor = normal[col][col];
        if divisor.abs() < 1e-20 {
            return vec![s[lo..=hi].iter().sum::<f32>() / (hi - lo + 1) as f32; 1153];
        }
        for value in &mut normal[col][col..] {
            *value /= divisor;
        }
        let pivot_row = normal[col];
        for (row, values) in normal.iter_mut().enumerate() {
            if row != col {
                let factor = values[col];
                for (value, pivot) in values[col..].iter_mut().zip(&pivot_row[col..]) {
                    *value -= factor * pivot;
                }
            }
        }
    }
    (0..1153)
        .map(|i| {
            let x = (i as f64 - middle as f64) / 1000.0;
            let y = (0..5).rev().fold(0.0, |v, j| v * x + normal[j][5]);
            10.0_f64.powf((y + 0.65) / 10.0) as f32
        })
        .collect()
}
// Abramowitz-Stegun erf approximation, maximum error <1.5e-7.
fn erf(x: f64) -> f64 {
    let sign = x.signum();
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * x);
    sign * (1.0
        - (((((1.061405429 * t - 1.453152027) * t + 1.421413741) * t - 0.284496736) * t
            + 0.254829592)
            * t)
            * (-x * x).exp())
}
fn waveform(tones: &[u8; 105], frequency: f32) -> Vec<C> {
    let mut increments = vec![TAU as f64 * frequency as f64 / 12000.0; 60480];
    let c = std::f64::consts::PI * (2.0 / std::f64::consts::LN_2).sqrt();
    let pulse: Vec<f64> = (1..=1728)
        .map(|i| {
            let t = (i as f64 - 864.0) / 576.0;
            0.5 * (erf(c * (t + 0.5)) - erf(c * (t - 0.5)))
        })
        .collect();
    for symbol in 0..103 {
        for (i, p) in pulse.iter().enumerate() {
            increments[symbol * 576 + i] +=
                std::f64::consts::TAU / 576.0 * p * tones[symbol + 1] as f64;
        }
    }
    let mut phase = 0.0_f64;
    increments
        .iter()
        .enumerate()
        .map(|(i, increment)| {
            let envelope = if i < 576 {
                0.5 * (1.0 - (PI * i as f32 / 576.0).cos())
            } else if i >= 59904 {
                0.5 * (1.0 + (PI * (i - 59904) as f32 / 576.0).cos())
            } else {
                1.0
            };
            let value = C::from_polar(envelope, phase as f32);
            phase = (phase + increment) % std::f64::consts::TAU;
            value
        })
        .collect()
}
