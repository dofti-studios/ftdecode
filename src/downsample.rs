// SPDX-License-Identifier: GPL-3.0-or-later
//! Candidate-frequency downsampling for FT8 and FT4.
//!
//! Ported from WSJT-X `lib/ft8/ft8_downsample.f90` and
//! `lib/ft4/ft4_downsample.f90` at commit
//! ccdfaf3c1c109010d15399674ce278167cfde848 (GPL-3.0-or-later).

use std::{error::Error, fmt};

use crate::fft::{Complex32, ComplexFft, Direction, FftError, RealForward};

const SAMPLE_RATE: f32 = 12_000.0;
const FT8_INPUT_LEN: usize = 180_000;
const FT8_FFT_LEN: usize = 192_000;
const FT8_OUTPUT_LEN: usize = 3_200;
const FT4_INPUT_LEN: usize = 72_576;
const FT4_OUTPUT_LEN: usize = 4_032;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// An invalid buffer, sample, candidate frequency, or preparation state.
pub enum DownsampleError {
    InvalidInputLength { expected: usize, actual: usize },
    InvalidOutputLength { expected: usize, actual: usize },
    InvalidSample { index: usize },
    InvalidFrequency,
    NotPrepared,
    Fft(FftError),
}

impl fmt::Display for DownsampleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInputLength { expected, actual } => {
                write!(formatter, "expected {expected} input samples, got {actual}")
            }
            Self::InvalidOutputLength { expected, actual } => {
                write!(
                    formatter,
                    "expected {expected} output samples, got {actual}"
                )
            }
            Self::InvalidSample { index } => {
                write!(formatter, "invalid PCM sample at index {index}")
            }
            Self::InvalidFrequency => {
                formatter.write_str("frequency must be finite and between 0 and 6000 Hz")
            }
            Self::NotPrepared => formatter.write_str("audio has not been prepared"),
            Self::Fft(error) => write!(formatter, "FFT failed: {error}"),
        }
    }
}

impl Error for DownsampleError {}

impl From<FftError> for DownsampleError {
    fn from(error: FftError) -> Self {
        Self::Fft(error)
    }
}

fn validate_input(input: &[f32], expected: usize, bound: f32) -> Result<(), DownsampleError> {
    if input.len() != expected {
        return Err(DownsampleError::InvalidInputLength {
            expected,
            actual: input.len(),
        });
    }
    if let Some((index, _)) = input
        .iter()
        .enumerate()
        .find(|(_, sample)| !sample.is_finite() || sample.abs() > bound)
    {
        return Err(DownsampleError::InvalidSample { index });
    }
    Ok(())
}

fn validate_extract(
    frequency_hz: f32,
    output_len: usize,
    expected: usize,
) -> Result<(), DownsampleError> {
    if !frequency_hz.is_finite() || !(0.0..=6_000.0).contains(&frequency_hz) {
        return Err(DownsampleError::InvalidFrequency);
    }
    if output_len != expected {
        return Err(DownsampleError::InvalidOutputLength {
            expected,
            actual: output_len,
        });
    }
    Ok(())
}

/// Reusable 12 kHz FT8 downsampler producing 3,200 complex samples at 200 Hz.
pub struct Ft8Downsampler {
    forward: RealForward,
    inverse: ComplexFft,
    input: Vec<f32>,
    spectrum: Vec<Complex32>,
    work: Vec<Complex32>,
    taper: [f32; 101],
    prepared: bool,
}

impl Ft8Downsampler {
    /// Internal cancellation residuals may exceed the original PCM range.
    pub(crate) fn prepare_residual(&mut self, input: &[f32]) -> Result<(), DownsampleError> {
        self.prepare_with_bound(input, 1.0e12)
    }

    /// Creates an empty downsampler with reusable FFT plans and work buffers.
    pub fn new() -> Self {
        let taper =
            std::array::from_fn(|i| 0.5 * (1.0 + (i as f32 * std::f32::consts::PI / 100.0).cos()));
        Self {
            forward: RealForward::new(FT8_FFT_LEN).expect("fixed nonzero FFT length"),
            inverse: ComplexFft::new(FT8_OUTPUT_LEN, Direction::Inverse)
                .expect("fixed nonzero FFT length"),
            input: vec![0.0; FT8_FFT_LEN],
            spectrum: vec![Complex32::new(0.0, 0.0); FT8_FFT_LEN / 2 + 1],
            work: vec![Complex32::new(0.0, 0.0); FT8_OUTPUT_LEN],
            taper,
            prepared: false,
        }
    }

    /// Invalidates the prepared audio spectrum.
    pub fn reset(&mut self) {
        self.prepared = false;
    }

    /// Prepares exactly 180,000 finite PCM-scale samples (`|sample| <= 32768`).
    pub fn prepare(&mut self, input: &[f32]) -> Result<(), DownsampleError> {
        self.prepare_with_bound(input, 32768.0)
    }

    fn prepare_with_bound(&mut self, input: &[f32], bound: f32) -> Result<(), DownsampleError> {
        self.prepared = false;
        validate_input(input, FT8_INPUT_LEN, bound)?;
        self.input[..FT8_INPUT_LEN].copy_from_slice(input);
        self.input[FT8_INPUT_LEN..].fill(0.0);
        self.forward
            .transform(&mut self.input, &mut self.spectrum)?;
        self.prepared = true;
        Ok(())
    }

    /// Extracts a 0–6,000 Hz candidate, rounded to the native FFT bin.
    pub fn extract(
        &mut self,
        frequency_hz: f32,
        output: &mut [Complex32],
    ) -> Result<(), DownsampleError> {
        validate_extract(frequency_hz, output.len(), FT8_OUTPUT_LEN)?;
        if !self.prepared {
            return Err(DownsampleError::NotPrepared);
        }

        let df = SAMPLE_RATE / FT8_FFT_LEN as f32;
        let baud = SAMPLE_RATE / 1_920.0;
        let center = (frequency_hz / df).round() as isize;
        let upper = ((frequency_hz + 8.5 * baud) / df)
            .round()
            .min((FT8_FFT_LEN / 2) as f32) as usize;
        let lower = ((frequency_hz - 1.5 * baud) / df).round().max(1.0) as usize;
        self.work.fill(Complex32::new(0.0, 0.0));
        let count = upper - lower + 1;
        self.work[..count].copy_from_slice(&self.spectrum[lower..=upper]);
        for i in 0..=100 {
            self.work[i] *= self.taper[100 - i];
        }
        for i in 0..=100 {
            self.work[count - 101 + i] *= self.taper[i];
        }
        let shift = (center - lower as isize).rem_euclid(FT8_OUTPUT_LEN as isize) as usize;
        self.work.rotate_left(shift);
        self.inverse.transform(&mut self.work)?;
        let scale = 1.0 / ((FT8_FFT_LEN * FT8_OUTPUT_LEN) as f32).sqrt();
        for (destination, value) in output.iter_mut().zip(&self.work) {
            *destination = *value * scale;
        }
        Ok(())
    }
}

impl Default for Ft8Downsampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Reusable 12 kHz FT4 downsampler producing 4,032 complex samples at 666⅔ Hz.
pub struct Ft4Downsampler {
    forward: RealForward,
    inverse: ComplexFft,
    input: Vec<f32>,
    spectrum: Vec<Complex32>,
    work: Vec<Complex32>,
    window: Vec<f32>,
    prepared: bool,
}

impl Ft4Downsampler {
    /// Internal cancellation residuals may exceed the original PCM range.
    pub(crate) fn prepare_residual(&mut self, input: &[f32]) -> Result<(), DownsampleError> {
        self.prepare_with_bound(input, 1.0e12)
    }

    /// Creates an empty downsampler with reusable FFT plans and work buffers.
    pub fn new() -> Self {
        let df = SAMPLE_RATE / FT4_INPUT_LEN as f32;
        let baud = SAMPLE_RATE / 576.0;
        let transition = (0.5 * baud / df) as usize;
        let flat = (4.0 * baud / df) as usize;
        let mut window = vec![0.0; FT4_OUTPUT_LEN];
        for i in 0..transition {
            window[i] = 0.5
                * (1.0
                    + (std::f32::consts::PI * (transition - 1 - i) as f32 / transition as f32)
                        .cos());
            window[transition + flat + i] =
                0.5 * (1.0 + (std::f32::consts::PI * i as f32 / transition as f32).cos());
        }
        window[transition..transition + flat].fill(1.0);
        window.rotate_left((baud / df) as usize);
        Self {
            forward: RealForward::new(FT4_INPUT_LEN).expect("fixed nonzero FFT length"),
            inverse: ComplexFft::new(FT4_OUTPUT_LEN, Direction::Inverse)
                .expect("fixed nonzero FFT length"),
            input: vec![0.0; FT4_INPUT_LEN],
            spectrum: vec![Complex32::new(0.0, 0.0); FT4_INPUT_LEN / 2 + 1],
            work: vec![Complex32::new(0.0, 0.0); FT4_OUTPUT_LEN],
            window,
            prepared: false,
        }
    }

    /// Invalidates the prepared audio spectrum.
    pub fn reset(&mut self) {
        self.prepared = false;
    }

    /// Prepares exactly 72,576 finite PCM-scale samples (`|sample| <= 32768`).
    pub fn prepare(&mut self, input: &[f32]) -> Result<(), DownsampleError> {
        self.prepare_with_bound(input, 32768.0)
    }

    fn prepare_with_bound(&mut self, input: &[f32], bound: f32) -> Result<(), DownsampleError> {
        self.prepared = false;
        validate_input(input, FT4_INPUT_LEN, bound)?;
        self.input.copy_from_slice(input);
        self.forward
            .transform(&mut self.input, &mut self.spectrum)?;
        self.prepared = true;
        Ok(())
    }

    /// Extracts a 0–6,000 Hz candidate, rounded to the native FFT bin.
    pub fn extract(
        &mut self,
        frequency_hz: f32,
        output: &mut [Complex32],
    ) -> Result<(), DownsampleError> {
        validate_extract(frequency_hz, output.len(), FT4_OUTPUT_LEN)?;
        if !self.prepared {
            return Err(DownsampleError::NotPrepared);
        }

        let center = (frequency_hz / (SAMPLE_RATE / FT4_INPUT_LEN as f32)).round() as isize;
        self.work.fill(Complex32::new(0.0, 0.0));
        for offset in 0..=FT4_OUTPUT_LEN / 2 {
            let upper = center + offset as isize;
            if (0..self.spectrum.len() as isize).contains(&upper) {
                self.work[offset] = self.spectrum[upper as usize];
            }
            if offset != 0 {
                let lower = center - offset as isize;
                if (0..self.spectrum.len() as isize).contains(&lower) {
                    self.work[FT4_OUTPUT_LEN - offset] = self.spectrum[lower as usize];
                }
            }
        }
        for (value, window) in self.work.iter_mut().zip(&self.window) {
            *value = (*value * *window) / FT4_OUTPUT_LEN as f32;
        }
        self.inverse.transform(&mut self.work)?;
        output.copy_from_slice(&self.work);
        Ok(())
    }
}

impl Default for Ft4Downsampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod residual_tests {
    use super::*;
    #[test]
    fn subtraction_residual_can_exceed_pcm_bounds_without_clipping() {
        let mut ft8 = Ft8Downsampler::new();
        let mut data = vec![0.0; FT8_INPUT_LEN];
        data[0] = 40000.0;
        assert!(ft8.prepare(&data).is_err());
        assert!(ft8.prepare_residual(&data).is_ok());
        let mut ft4 = Ft4Downsampler::new();
        data.truncate(FT4_INPUT_LEN);
        assert!(ft4.prepare(&data).is_err());
        assert!(ft4.prepare_residual(&data).is_ok());
        data[0] = f32::INFINITY;
        assert!(ft4.prepare_residual(&data).is_err());
    }
}
