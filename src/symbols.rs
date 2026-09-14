// SPDX-License-Identifier: GPL-3.0-or-later
//! Symbol layouts for demodulation and internal signal subtraction.
//! Ported from WSJT-X `lib/ft8/genft8.f90` and `lib/ft4/genft4.f90`
//! at ccdfaf3c1c109010d15399674ce278167cfde848; see NOTICE.md.

/// XOR the FT4 scrambling sequence. The same operation restores source bits.
pub fn scramble_ft4(message: &[bool; 77]) -> [bool; 77] {
    const MASK: &[u8; 77] =
        b"01001010010111101000100110110100101100001000101001111001010101011011111000101";
    std::array::from_fn(|i| message[i] ^ (MASK[i] == b'1'))
}

/// FT8's 79 sync/data symbols, using the reference's Gray mapping.
pub fn ft8(codeword: &[bool; 174]) -> [u8; 79] {
    const SYNC: [u8; 7] = [3, 1, 4, 0, 6, 5, 2];
    const GRAY: [u8; 8] = [0, 1, 3, 2, 5, 6, 4, 7];
    let mut tones = [0; 79];
    for offset in [0, 36, 72] {
        tones[offset..offset + 7].copy_from_slice(&SYNC);
    }
    for (i, bits) in codeword.as_chunks::<3>().0.iter().enumerate() {
        let index =
            (usize::from(bits[0]) << 2) | (usize::from(bits[1]) << 1) | usize::from(bits[2]);
        tones[7 + i + (i / 29) * 7] = GRAY[index];
    }
    tones
}

/// FT4's 103 sync/data symbols with a zero-valued ramp symbol at each end.
pub fn ft4(codeword: &[bool; 174]) -> [u8; 105] {
    const SYNC: [[u8; 4]; 4] = [[0, 1, 3, 2], [1, 0, 2, 3], [2, 3, 1, 0], [3, 2, 0, 1]];
    const GRAY: [u8; 4] = [0, 1, 3, 2];
    let mut tones = [0; 105];
    for (i, sync) in SYNC.iter().enumerate() {
        tones[1 + i * 33..5 + i * 33].copy_from_slice(sync);
    }
    for (i, bits) in codeword.as_chunks::<2>().0.iter().enumerate() {
        let index = (usize::from(bits[0]) << 1) | usize::from(bits[1]);
        tones[5 + i + (i / 29) * 4] = GRAY[index];
    }
    tones
}
