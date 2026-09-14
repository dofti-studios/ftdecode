// SPDX-License-Identifier: GPL-3.0-or-later
//! Channel selection and bounded conversion to 12 kHz PCM-scale floats.
use crate::wav::{WavFormat, WavReader};
use std::io::{self, Read, Seek};

const OUTPUT_RATE: u64 = 12_000;
const INPUT_CHUNK: usize = 1024;
const FILTER_PHASES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Channel {
    Left,
    Right,
    Mix,
}

/// Constant-ratio windowed-sinc FIR. Integer positions prevent timing drift on
/// long recordings. The centered filter reads ahead and zero-extends endpoints;
/// it adds no delay to the output timeline.
struct SincState {
    rate: u32,
    position: u64,
    phase: u32,
    taps: usize,
    kernels: Vec<Vec<f32>>,
    buffer: Vec<f32>,
    buffer_start: u64,
    source_eof: bool,
}

impl SincState {
    fn new(rate: u32) -> Self {
        // An 80-output-sample support and 5.5 kHz cutoff preserve the decoder's
        // 0–5 kHz passband while suppressing energy above the 6 kHz Nyquist limit.
        let radius = (40 * rate).div_ceil(OUTPUT_RATE as u32) as usize;
        let taps = 2 * radius + 2;
        let cutoff = 5500.0 / f64::from(rate);
        let kernels = (0..=FILTER_PHASES)
            .map(|phase| {
                let fraction = phase as f64 / FILTER_PHASES as f64;
                let mut kernel = (0..taps)
                    .map(|tap| {
                        let x = tap as f64 - radius as f64 - fraction;
                        let angle = std::f64::consts::PI * x / radius as f64;
                        let window = if x.abs() <= radius as f64 {
                            0.35875
                                + 0.48829 * angle.cos()
                                + 0.14128 * (2.0 * angle).cos()
                                + 0.01168 * (3.0 * angle).cos()
                        } else {
                            0.0
                        };
                        let sinc = if x == 0.0 {
                            2.0 * cutoff
                        } else {
                            (std::f64::consts::TAU * cutoff * x).sin() / (std::f64::consts::PI * x)
                        };
                        sinc * window
                    })
                    .collect::<Vec<_>>();
                let sum: f64 = kernel.iter().sum();
                kernel.iter_mut().for_each(|value| *value /= sum);
                kernel.into_iter().map(|value| value as f32).collect()
            })
            .collect();
        // Virtual buffer index zero is source index -radius. This avoids signed
        // indexing at the start without limiting the duration of a recording.
        let mut buffer = Vec::with_capacity(taps + INPUT_CHUNK);
        buffer.resize(radius, 0.0);
        Self {
            rate,
            position: 0,
            phase: 0,
            taps,
            kernels,
            buffer,
            buffer_start: 0,
            source_eof: false,
        }
    }

    fn next<R: Read + Seek>(
        &mut self,
        wav: &mut WavReader<R>,
        channel: Channel,
    ) -> io::Result<f32> {
        let mut offset = (self.position - self.buffer_start) as usize;
        if offset + self.taps > self.buffer.len() {
            self.buffer.drain(..offset);
            self.buffer_start = self.position;
            offset = 0;
            while self.buffer.len() < self.taps {
                if self.source_eof {
                    self.buffer.resize(self.taps, 0.0);
                    break;
                }
                let input = wav.read_float_frames(INPUT_CHUNK)?;
                let mono = select_channel(wav.format().channels, channel, &input);
                self.source_eof = mono.len() < INPUT_CHUNK;
                self.buffer.extend(mono);
            }
        }
        let scaled = self.phase as usize * FILTER_PHASES;
        let phase = scaled / OUTPUT_RATE as usize;
        let fraction = (scaled % OUTPUT_RATE as usize) as f32 / OUTPUT_RATE as f32;
        let left = &self.kernels[phase];
        let right = &self.kernels[phase + 1];
        let samples = &self.buffer[offset..offset + self.taps];
        // Independent accumulation lanes allow safe Rust to auto-vectorize.
        let mut sums = [0.0f32; 8];
        let (sample_chunks, sample_tail) = samples.as_chunks::<8>();
        let (left_chunks, left_tail) = left.as_chunks::<8>();
        let (right_chunks, right_tail) = right.as_chunks::<8>();
        if fraction == 0.0 {
            for (samples, weights) in sample_chunks.iter().zip(left_chunks) {
                for lane in 0..8 {
                    sums[lane] += samples[lane] * weights[lane];
                }
            }
        } else {
            for ((samples, left), right) in sample_chunks.iter().zip(left_chunks).zip(right_chunks)
            {
                for lane in 0..8 {
                    let weight = left[lane] + fraction * (right[lane] - left[lane]);
                    sums[lane] += samples[lane] * weight;
                }
            }
        }
        for (i, &sample) in sample_tail.iter().enumerate() {
            sums[i] += sample * (left_tail[i] + fraction * (right_tail[i] - left_tail[i]));
        }
        let advance = self.phase + self.rate;
        self.position += u64::from(advance / OUTPUT_RATE as u32);
        self.phase = advance % OUTPUT_RATE as u32;
        Ok(sums.iter().sum())
    }
}

enum Conversion {
    Bypass,
    Sinc(SincState),
}

pub struct ConvertedWav<R> {
    wav: WavReader<R>,
    channel: Channel,
    conversion: Conversion,
    sample_count: u64,
    emitted: u64,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl<R: Read + Seek> ConvertedWav<R> {
    pub fn new(wav: WavReader<R>, channel: Channel) -> io::Result<Self> {
        let format = wav.format();
        if format.channels == 1 && channel == Channel::Right {
            return Err(invalid("right channel requested from a mono WAV"));
        }
        let sample_count = wav
            .sample_count()
            .checked_mul(OUTPUT_RATE)
            .ok_or_else(|| invalid("converted WAV duration is too large"))?
            / u64::from(format.sample_rate);
        let conversion = if format.sample_rate == OUTPUT_RATE as u32 {
            Conversion::Bypass
        } else {
            Conversion::Sinc(SincState::new(format.sample_rate))
        };
        Ok(Self {
            wav,
            channel,
            conversion,
            sample_count,
            emitted: 0,
        })
    }

    pub fn sample_count(&self) -> u64 {
        self.sample_count
    }

    pub fn format(&self) -> WavFormat {
        self.wav.format()
    }

    /// Reads at most one second of 12 kHz PCM-scale floating-point audio.
    pub fn read_samples(&mut self, maximum: usize) -> io::Result<Vec<f32>> {
        if maximum == 0 {
            return Ok(Vec::new());
        }
        let wanted = maximum
            .min(OUTPUT_RATE as usize)
            .min((self.sample_count - self.emitted) as usize);
        if wanted == 0 {
            // Even samples that do not produce output must be validated.
            while !self.wav.read_float_frames(INPUT_CHUNK)?.is_empty() {}
            return Ok(Vec::new());
        }
        let output = match &mut self.conversion {
            Conversion::Bypass => {
                let interleaved = self.wav.read_float_frames(wanted)?;
                select_channel(self.wav.format().channels, self.channel, &interleaved)
            }
            Conversion::Sinc(state) => {
                let mut output = Vec::with_capacity(wanted);
                for _ in 0..wanted {
                    output.push(state.next(&mut self.wav, self.channel)?);
                }
                output
            }
        };
        self.emitted += output.len() as u64;
        Ok(output)
    }
}

fn select_channel(channels: u16, channel: Channel, interleaved: &[f32]) -> Vec<f32> {
    if channels == 1 {
        return interleaved.to_vec();
    }
    let offset = usize::from(channel == Channel::Right);
    match channel {
        Channel::Left | Channel::Right => interleaved
            .as_chunks::<2>()
            .0
            .iter()
            .map(|frame| frame[offset])
            .collect(),
        Channel::Mix => interleaved
            .as_chunks::<2>()
            .0
            .iter()
            .map(|frame| (frame[0] + frame[1]) * 0.5)
            .collect(),
    }
}
