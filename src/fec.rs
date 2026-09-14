// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared (174, 91) FEC. Bits are in transmitted, most-significant-first order.
//! Ported from WSJT-X `encode174_91.f90`, `get_crc14.f90`,
//! `bpdecode174_91.f90`, `decode174_91.f90` and LDPC tables
//! at ccdfaf3c1c109010d15399674ce278167cfde848; see NOTICE.md.

use crate::tables::{MN, NM};

/// CRC of 77 payload bits padded to 82 bits, followed by 14 zero CRC bits.
pub fn crc14(message: &[bool; 77]) -> u16 {
    let mut remainder = 0_u16;
    for bit in message.iter().copied().chain([false; 19]) {
        let high = remainder & 0x2000 != 0;
        remainder = ((remainder << 1) | u16::from(bit)) & 0x3fff;
        if high {
            remainder ^= 0x2757;
        }
    }
    remainder
}

/// Append CRC and systematic LDPC parity. FT4 payloads must be scrambled first.
pub fn encode(message: &[bool; 77]) -> [bool; 174] {
    let crc = crc14(message);
    let mut codeword = [false; 174];
    codeword[..77].copy_from_slice(message);
    for i in 0..14 {
        codeword[77 + i] = crc & (1 << (13 - i)) != 0;
    }
    let packed = codeword[..91]
        .iter()
        .fold(0_u128, |n, &bit| (n << 1) | u128::from(bit));
    for (i, row) in crate::tables::GENERATOR.iter().enumerate() {
        // Each upstream hex row has 91 significant bits plus one padding bit.
        codeword[91 + i] = (packed & (row >> 1)).count_ones() % 2 != 0;
    }
    codeword
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub message: [bool; 77],
    pub codeword: [bool; 174],
    pub iterations: u8,
    pub hard_errors: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidLlr {
    pub index: usize,
}

impl std::fmt::Display for InvalidLlr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "non-finite log likelihood at bit {}", self.index)
    }
}

impl std::error::Error for InvalidLlr {}

/// Unassisted log-domain belief propagation, ported from `bpdecode174_91.f90`.
/// Positive likelihoods favor one; negative likelihoods favor zero.
/// `max_iterations = 0` checks only the initial hard decision. Work is bounded
/// by the supplied u8 iteration budget and the reference's stagnation cutoff.
///
/// A returned payload passed parity and CRC, not the message/audio acceptance
/// checks needed by a complete decoder. FT4 still needs descrambling.
/// Unlike upstream's low-level routine, an all-zero input returns no result.
pub fn decode(llr: &[f32; 174], max_iterations: u8) -> Result<Option<Decoded>, InvalidLlr> {
    decode_ap(llr, &[false; 174], max_iterations)
}

/// Belief propagation with fixed AP posteriors and no OSD fallback.
pub fn decode_ap(
    llr: &[f32; 174],
    mask: &[bool; 174],
    max_iterations: u8,
) -> Result<Option<Decoded>, InvalidLlr> {
    decode_with_observer(llr, mask, max_iterations, |_, _| {})
}

fn decode_with_observer(
    llr: &[f32; 174],
    mask: &[bool; 174],
    max_iterations: u8,
    mut observe: impl FnMut(u8, &[f32; 174]),
) -> Result<Option<Decoded>, InvalidLlr> {
    if let Some(index) = llr.iter().position(|x| !x.is_finite()) {
        return Err(InvalidLlr { index });
    }
    if llr.iter().all(|&x| x == 0.0) {
        return Ok(None);
    }
    let mut to_variable = [[0_f32; 3]; 174];
    let mut tanh_to_check = [[0_f32; 7]; 83];
    let mut last_checks = 0;
    let mut stagnant = 0;

    for iteration in 0..=max_iterations {
        let posterior: [f32; 174] = std::array::from_fn(|i| {
            if mask[i] {
                llr[i]
            } else {
                llr[i] + (to_variable[i][0] + to_variable[i][1] + to_variable[i][2])
            }
        });
        observe(iteration, &posterior);
        let codeword: [bool; 174] = posterior.map(|x| x > 0.0);
        let checks = NM
            .iter()
            .filter(|row| {
                row.iter()
                    .filter(|&&i| i < 174)
                    .fold(false, |parity, &i| parity ^ codeword[i])
            })
            .count();
        if checks == 0 {
            let message: [bool; 77] = std::array::from_fn(|i| codeword[i]);
            let received_crc = codeword[77..91]
                .iter()
                .fold(0_u16, |n, &b| (n << 1) | u16::from(b));
            if crc14(&message) == received_crc {
                let hard_errors = codeword
                    .iter()
                    .zip(llr)
                    .filter(|(bit, x)| if **bit { **x < 0.0 } else { **x > 0.0 })
                    .count();
                return Ok(Some(Decoded {
                    message,
                    codeword,
                    iterations: iteration,
                    hard_errors,
                }));
            }
        }
        if iteration > 0 {
            stagnant = if checks < last_checks {
                0
            } else {
                stagnant + 1
            };
            if stagnant >= 5 && iteration >= 10 && checks > 15 {
                return Ok(None);
            }
        }
        last_checks = checks;
        if iteration == max_iterations {
            break;
        }

        // Messages to a check exclude what that check previously supplied.
        for (check, row) in NM.iter().enumerate() {
            for (edge, &bit) in row.iter().enumerate().filter(|(_, bit)| **bit < 174) {
                let neighbor = MN[bit]
                    .iter()
                    .position(|&c| c == check)
                    .expect("valid LDPC table");
                let value = posterior[bit] - to_variable[bit][neighbor];
                tanh_to_check[check][edge] = (-value / 2.0).tanh();
            }
        }
        for (bit, neighbors) in MN.iter().enumerate() {
            for (edge, &check) in neighbors.iter().enumerate() {
                let mut product = 1.0;
                for (slot, &other) in NM[check].iter().enumerate() {
                    if other < 174 && other != bit {
                        product *= tanh_to_check[check][slot];
                    }
                }
                to_variable[bit][edge] = 2.0 * approximate_atanh(-product);
            }
        }
    }
    Ok(None)
}

// Preserve WSJT-X lib/platanh.f90's approximation, including saturation.
fn approximate_atanh(x: f32) -> f32 {
    let z = x.abs();
    let magnitude = if z <= 0.664 {
        return x / 0.83;
    } else if z <= 0.9217 {
        (z - 0.4064) / 0.322
    } else if z <= 0.9951 {
        (z - 0.8378) / 0.0524
    } else if z <= 0.9998 {
        (z - 0.9914) / 0.0012
    } else {
        7.0
    };
    magnitude.copysign(x)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeMethod {
    Bp,
    Osd,
    /// Native hypothesis-list decoding (FT8 a7/a8).
    List,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HybridDecoded {
    /// `iterations` is the BP iteration at success or at the start of fallback.
    /// For OSD, `hard_errors` is measured against the selected OSD input
    /// (channel or accumulated posteriors), matching the native hybrid routine.
    pub decoded: Decoded,
    pub method: DecodeMethod,
    /// Distance is always measured against the original channel likelihoods.
    pub distance: f32,
    /// One-based OSD attempt index, or None when BP succeeded.
    pub osd_pass: Option<u8>,
}

/// Native `decode174_91.f90` BP/OSD fallback with AP off and K fixed at 91.
/// BP runs for up to 30 iterations with its stagnation cutoff. `max_osd = 0`
/// tries OSD once on channel likelihoods; 1..=3 tries accumulated posteriors
/// from iterations 0..=1, 0..=2 and 0..=3, stopping at the first success.
/// `depth` accepts native depths 0..=2. Use [`decode`] for BP without fallback.
///
/// As in the native hybrid routine, an OSD candidate must have at least one
/// mismatch against its OSD input. All results pass CRC, but still need the
/// receive path's message/audio checks and (for FT4) payload descrambling.
pub fn decode_hybrid(
    llr: &[f32; 174],
    max_osd: u8,
    depth: u8,
) -> Result<Option<HybridDecoded>, crate::osd::DecodeError> {
    decode_hybrid_ap(llr, &[false; 174], max_osd, depth)
}

/// Native AP-assisted hybrid decoding with K fixed at 91.
///
/// A true mask bit freezes its BP posterior at the supplied channel likelihood
/// and excludes that bit from OSD trial flips when it belongs to the most
/// reliable information basis. The caller supplies the assumed sign and
/// magnitude in `llr`; the mask does not itself supply or strengthen evidence.
/// Bits outside OSD's information basis are not independently constrained,
/// matching the native decoder. Validation and budgets match [`decode_hybrid`].
pub fn decode_hybrid_ap(
    llr: &[f32; 174],
    mask: &[bool; 174],
    max_osd: u8,
    depth: u8,
) -> Result<Option<HybridDecoded>, crate::osd::DecodeError> {
    use crate::osd::{self, DecodeError};
    osd::validate(llr, depth)?;
    if max_osd > 3 {
        return Err(DecodeError::UnsupportedMaxOsd(max_osd));
    }
    let mut saved = [[0.0_f32; 174]; 3];
    let mut sum = [0.0_f32; 174];
    let mut last_iteration = 0;
    let bp = decode_with_observer(llr, mask, 30, |iteration, posterior| {
        last_iteration = iteration;
        if iteration <= max_osd && max_osd > 0 {
            for (total, value) in sum.iter_mut().zip(posterior) {
                *total += value;
            }
            if iteration > 0 {
                saved[usize::from(iteration - 1)] = sum;
            }
        }
    })
    .map_err(DecodeError::InvalidLlr)?;
    let channel_distance = |word: &[bool; 174]| {
        word.iter()
            .zip(llr)
            .filter(|(bit, value)| **bit != (**value >= 0.0))
            .map(|(_, value)| value.abs())
            .sum()
    };
    if let Some(decoded) = bp {
        return Ok(Some(HybridDecoded {
            distance: channel_distance(&decoded.codeword),
            decoded,
            method: DecodeMethod::Bp,
            osd_pass: None,
        }));
    }
    if max_osd == 0 {
        saved[0] = *llr;
    }
    for pass in 0..max_osd.max(1) {
        if let Some(candidate) = osd::decode_ap(&saved[usize::from(pass)], mask, depth)? {
            if candidate.hard_errors == 0 {
                continue;
            }
            return Ok(Some(HybridDecoded {
                distance: channel_distance(&candidate.codeword),
                decoded: Decoded {
                    message: candidate.message,
                    codeword: candidate.codeword,
                    iterations: last_iteration,
                    hard_errors: candidate.hard_errors,
                },
                method: DecodeMethod::Osd,
                osd_pass: Some(pass + 1),
            }));
        }
    }
    Ok(None)
}
