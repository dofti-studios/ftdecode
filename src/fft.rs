// SPDX-License-Identifier: GPL-3.0-or-later
//! Reusable, unnormalized FFT plans used by the decoder DSP.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use rustfft::{Fft, FftPlanner};

pub use rustfft::num_complex::Complex32;

/// Direction of a complex transform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Forward,
    Inverse,
}

/// An invalid FFT length or execution buffer shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FftError {
    ZeroLength,
    InputLength { expected: usize, actual: usize },
    OutputLength { expected: usize, actual: usize },
}

impl fmt::Display for FftError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroLength => formatter.write_str("FFT length must be nonzero"),
            Self::InputLength { expected, actual } => {
                write!(formatter, "expected input length {expected}, got {actual}")
            }
            Self::OutputLength { expected, actual } => {
                write!(formatter, "expected output length {expected}, got {actual}")
            }
        }
    }
}

impl Error for FftError {}

/// A reusable in-place complex FFT.
pub struct ComplexFft {
    plan: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
}

impl ComplexFft {
    pub fn new(len: usize, direction: Direction) -> Result<Self, FftError> {
        if len == 0 {
            return Err(FftError::ZeroLength);
        }

        let mut planner = FftPlanner::new();
        let plan = match direction {
            Direction::Forward => planner.plan_fft_forward(len),
            Direction::Inverse => planner.plan_fft_inverse(len),
        };
        let scratch = vec![Complex32::new(0.0, 0.0); plan.get_inplace_scratch_len()];
        Ok(Self { plan, scratch })
    }

    pub fn transform(&mut self, values: &mut [Complex32]) -> Result<(), FftError> {
        if values.len() != self.plan.len() {
            return Err(FftError::InputLength {
                expected: self.plan.len(),
                actual: values.len(),
            });
        }

        self.plan.process_with_scratch(values, &mut self.scratch);
        Ok(())
    }
}

/// A reusable forward transform from real input to its nonnegative spectrum.
pub struct RealForward {
    len: usize,
    plan: ComplexFft,
    buffer: Vec<Complex32>,
    twiddles: Vec<Complex32>,
}

impl RealForward {
    pub fn new(len: usize) -> Result<Self, FftError> {
        if len == 0 {
            return Err(FftError::ZeroLength);
        }
        let even = len.is_multiple_of(2);
        let complex_len = if even { len / 2 } else { len };
        let twiddles = if even {
            (0..(len / 2).div_ceil(2))
                .map(|k| {
                    let angle = -std::f64::consts::TAU * k as f64 / len as f64;
                    Complex32::new(angle.cos() as f32, angle.sin() as f32)
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            len,
            plan: ComplexFft::new(complex_len, Direction::Forward)?,
            buffer: vec![Complex32::default(); if even { 0 } else { complex_len }],
            twiddles,
        })
    }

    pub fn transform(
        &mut self,
        input: &mut [f32],
        output: &mut [Complex32],
    ) -> Result<(), FftError> {
        if input.len() != self.len {
            return Err(FftError::InputLength {
                expected: self.len,
                actual: input.len(),
            });
        }
        if output.len() != self.len / 2 + 1 {
            return Err(FftError::OutputLength {
                expected: self.len / 2 + 1,
                actual: output.len(),
            });
        }
        if !self.len.is_multiple_of(2) {
            // Preserve support for arbitrary nonzero lengths, including one.
            for (value, &sample) in self.buffer.iter_mut().zip(input.iter()) {
                *value = Complex32::new(sample, 0.0);
            }
            self.plan.transform(&mut self.buffer)?;
            output.copy_from_slice(&self.buffer[..output.len()]);
            output[0].im = 0.0;
            return Ok(());
        }

        // Pack even samples as real and odd samples as imaginary components.
        // One N/2 complex FFT yields both real subsequences' spectra:
        // E[k] = (Z[k] + conj(Z[-k]))/2,
        // O[k] = (Z[k] - conj(Z[-k]))/(2i), X[k] = E[k] + W_N^k O[k].
        let half = self.len / 2;
        for (value, samples) in output[..half].iter_mut().zip(input.as_chunks::<2>().0) {
            *value = Complex32::new(samples[0], samples[1]);
        }
        self.plan.transform(&mut output[..half])?;
        let zero = output[0];
        output[0] = Complex32::new(zero.re + zero.im, 0.0);
        output[half] = Complex32::new(zero.re - zero.im, 0.0);
        // Recover conjugate pairs together, retaining both packed inputs before
        // overwriting them. This needs no additional even-length signal buffer.
        for k in 1..half.div_ceil(2) {
            let a = output[k];
            let b = output[half - k].conj();
            let difference = a - b;
            let odd = Complex32::new(difference.im, -difference.re);
            let rotated = self.twiddles[k] * odd;
            output[k] = (a + b + rotated) * 0.5;
            output[half - k] = ((a + b - rotated) * 0.5).conj();
        }
        if half.is_multiple_of(2) {
            output[half / 2] = output[half / 2].conj();
        }
        Ok(())
    }
}
