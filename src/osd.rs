// SPDX-License-Identifier: GPL-3.0-or-later
//! Ordered-statistics decoding of the (174, 91) code with optional AP masks.
//! Ported from WSJT-X `osd174_91.f90` and `indexx.f90` at
//! ccdfaf3c1c109010d15399674ce278167cfde848 (WSJT-X authors; see NOTICE.md).
//! K is fixed at 91: all 14 CRC bits detect errors.

use crate::fec::{InvalidLlr, crc14};

#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    pub message: [bool; 77],
    pub codeword: [bool; 174],
    /// Mismatches with the OSD input's hard decisions (zero favors one).
    pub hard_errors: usize,
    /// Native weighted search score. With a full information basis this is
    /// the sum of absolute input likelihoods at mismatching positions; the
    /// upstream metric is retained when its pivot window is exhausted.
    pub distance: f32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    InvalidLlr(InvalidLlr),
    UnsupportedDepth(u8),
    UnsupportedMaxOsd(u8),
    /// Input magnitudes cannot safely support bounded f32 sums.
    LikelihoodOverflow,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLlr(error) => error.fmt(f),
            Self::UnsupportedDepth(depth) => {
                write!(f, "unsupported OSD depth {depth}; supported: 0..=2")
            }
            Self::UnsupportedMaxOsd(passes) => {
                write!(f, "unsupported max_osd {passes}; supported: 0..=3")
            }
            Self::LikelihoodOverflow => write!(
                f,
                "likelihood magnitudes exceed safe f32 accumulation range"
            ),
        }
    }
}
impl std::error::Error for DecodeError {}

pub(crate) fn validate(llr: &[f32; 174], depth: u8) -> Result<(), DecodeError> {
    if depth > 2 {
        return Err(DecodeError::UnsupportedDepth(depth));
    }
    if let Some(index) = llr.iter().position(|x| !x.is_finite()) {
        return Err(DecodeError::InvalidLlr(InvalidLlr { index }));
    }
    // More headroom than the four accumulated posteriors and 174-term metric
    // need. Normal radio likelihoods are many orders of magnitude smaller.
    if llr.iter().any(|x| x.abs() > f32::MAX / 4096.0) {
        return Err(DecodeError::LikelihoodOverflow);
    }
    Ok(())
}

/// Decode with the native depths 0, 1 or 2. Depth 0 tests one codeword;
/// depth 1 tests 91 single flips; depth 2 also tests 4,095 double flips.
/// The native 40-parity-bit pruning thresholds are 12 and 10 respectively.
/// This is a bounded list search, not maximum-likelihood decoding. A result
/// passes CRC; audio/message acceptance and FT4 descrambling remain separate.
/// All-zero likelihoods return no result. Invalid settings are never clamped.
pub fn decode(llr: &[f32; 174], depth: u8) -> Result<Option<Decoded>, DecodeError> {
    decode_ap(llr, &[false; 174], depth)
}

/// Decode while excluding masked information-basis bits from trial flips.
/// The mask follows the same reliability sorting and pivot swaps as the bits.
/// As upstream, masked positions outside the information basis can change
/// during re-encoding; this is not a global codeword constraint. Assumed bit
/// signs and magnitudes come from `llr`. Other behavior matches [`decode`].
pub fn decode_ap(
    llr: &[f32; 174],
    mask: &[bool; 174],
    depth: u8,
) -> Result<Option<Decoded>, DecodeError> {
    validate(llr, depth)?;
    if llr.iter().all(|&x| x == 0.0) {
        return Ok(None);
    }
    let reliability = llr.map(f32::abs);
    let mut indices = indexx(&reliability);
    indices.reverse();
    // Each column is a 91-bit vector; row operations can therefore be carried
    // out with one u128 XOR per column, preserving the native pivot order.
    let mut columns: [u128; 174] = std::array::from_fn(|i| {
        let original = indices[i];
        if original < 91 {
            1 << original
        } else {
            // Upstream packed generator rows run in MSB-first message order.
            (0..91).fold(0, |column, bit| {
                column | (((crate::tables::GENERATOR[original - 91] >> (91 - bit)) & 1) << bit)
            })
        }
    });
    for pivot in 0..91 {
        let row_bit = 1_u128 << pivot;
        let Some(column) = (pivot..111).find(|&i| columns[i] & row_bit != 0) else {
            // Native leaves this row unpivoted and continues when its ad hoc
            // window is exhausted. Preserve that bounded behavior, including
            // the candidate-position mismatch term in the metric below.
            continue;
        };
        columns.swap(pivot, column);
        indices.swap(pivot, column);
        let eliminate = columns[pivot] ^ row_bit;
        for column in &mut columns {
            if *column & row_bit != 0 {
                *column ^= eliminate;
            }
        }
    }
    let rows: [Bits; 91] = std::array::from_fn(|i| Bits::from_fn(|j| columns[j] & (1 << i) != 0));
    let hard = Bits::from_fn(|i| llr[indices[i]] >= 0.0);
    let weights: [f32; 174] = std::array::from_fn(|i| reliability[indices[i]]);
    let mut c0 = Bits::default();
    for (i, row) in rows.iter().enumerate() {
        if hard.get(i) {
            c0 = c0.xor(*row);
        }
    }
    let mut best = c0;
    let mismatch = best.xor(hard);
    let mut hard_errors = mismatch.count();
    let mut distance = mismatch.distance(&weights, 0);
    if depth > 0 {
        let threshold = if depth == 1 { 12 } else { 10 };
        for first in (0..91).rev() {
            if mask[indices[first]] {
                continue;
            }
            let single = c0.xor(rows[first]);
            let single_errors = single.xor(hard);
            let first_distance = weights[first];
            let end = if depth == 1 { first } else { 0 };
            for second in (end..=first).rev() {
                if mask[indices[second]] {
                    continue;
                }
                let candidate = if second == first {
                    single
                } else {
                    single.xor(rows[second])
                };
                let errors = candidate.xor(hard);
                let flips = if second == first { 1 } else { 2 };
                if errors.first_parity_count() + flips > threshold {
                    continue;
                }
                let trial_distance = if second == first {
                    first_distance + single_errors.distance(&weights, 91)
                } else {
                    first_distance
                        + if errors.get(second) {
                            weights[second]
                        } else {
                            0.0
                        }
                        + errors.distance(&weights, 91)
                };
                if trial_distance < distance {
                    distance = trial_distance;
                    best = candidate;
                    hard_errors = errors.count();
                }
            }
        }
    }
    let mut codeword = [false; 174];
    for (i, &original) in indices.iter().enumerate() {
        codeword[original] = best.get(i);
    }
    let message = std::array::from_fn(|i| codeword[i]);
    let received_crc = codeword[77..91]
        .iter()
        .fold(0_u16, |crc, &b| (crc << 1) | u16::from(b));
    // Native encodes CRC failure by negating nhardmin, losing failure when
    // there are zero mismatches. Check CRC explicitly to reject that case.
    if crc14(&message) != received_crc {
        return Ok(None);
    }
    Ok(Some(Decoded {
        message,
        codeword,
        hard_errors,
        distance,
    }))
}

#[derive(Clone, Copy, Default)]
struct Bits([u64; 3]);
impl Bits {
    fn from_fn(mut bit: impl FnMut(usize) -> bool) -> Self {
        let mut result = Self::default();
        for i in 0..174 {
            if bit(i) {
                result.0[i / 64] |= 1 << (i % 64);
            }
        }
        result
    }
    fn get(self, i: usize) -> bool {
        self.0[i / 64] & (1 << (i % 64)) != 0
    }
    fn xor(self, other: Self) -> Self {
        Self(std::array::from_fn(|i| self.0[i] ^ other.0[i]))
    }
    fn count(self) -> usize {
        self.0.iter().map(|x| x.count_ones() as usize).sum()
    }
    fn first_parity_count(self) -> usize {
        ((self.0[1] >> 27).count_ones() + (self.0[2] & 7).count_ones()) as usize
    }
    fn distance(self, weights: &[f32; 174], start: usize) -> f32 {
        weights
            .iter()
            .enumerate()
            .skip(start)
            .filter(|(i, _)| self.get(*i))
            .map(|(_, w)| w)
            .sum()
    }
}

// Numerical Recipes indexx as used by WSJT-X. Its unstable equal-key order
// affects the reliability basis and search pruning; Rust sort is not equivalent.
fn indexx(values: &[f32; 174]) -> [usize; 174] {
    let mut order: [usize; 175] = std::array::from_fn(|i| i.saturating_sub(1));
    let mut left = 1;
    let mut right = 174;
    let mut stack = Vec::with_capacity(50);
    loop {
        if right - left < 7 {
            for j in left + 1..=right {
                let item = order[j];
                let value = values[item];
                let mut i = j - 1;
                while i >= 1 && values[order[i]] > value {
                    order[i + 1] = order[i];
                    i -= 1;
                }
                order[i + 1] = item;
            }
            let Some((next_left, next_right)) = stack.pop() else {
                break;
            };
            left = next_left;
            right = next_right;
        } else {
            order.swap((left + right) / 2, left + 1);
            if values[order[left + 1]] > values[order[right]] {
                order.swap(left + 1, right);
            }
            if values[order[left]] > values[order[right]] {
                order.swap(left, right);
            }
            if values[order[left + 1]] > values[order[left]] {
                order.swap(left + 1, left);
            }
            let mut i = left + 1;
            let mut j = right;
            let pivot = order[left];
            let value = values[pivot];
            loop {
                i += 1;
                while values[order[i]] < value {
                    i += 1;
                }
                j -= 1;
                while values[order[j]] > value {
                    j -= 1;
                }
                if j < i {
                    break;
                }
                order.swap(i, j);
            }
            order[left] = order[j];
            order[j] = pivot;
            if right - i + 1 >= j - left {
                stack.push((i, right));
                right = j - 1;
            } else {
                stack.push((left, j - 1));
                left = i;
            }
        }
    }
    std::array::from_fn(|i| order[i + 1])
}

#[cfg(test)]
mod tests {
    use super::indexx;

    #[test]
    fn reliability_order_matches_native_including_equal_keys() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/osd-vectors.json")).unwrap();
        for input in vectors["inputs"].as_array().unwrap() {
            let values = std::array::from_fn(|i| (input["llr"][i].as_f64().unwrap() as f32).abs());
            let expected: [usize; 174] =
                std::array::from_fn(|i| input["reliability_order"][i].as_u64().unwrap() as usize);
            assert_eq!(indexx(&values), expected, "{}", input["id"]);
        }
    }
}
