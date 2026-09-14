// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded RIFF/WAVE reader for decoder-compatible PCM input.
use std::io::{self, Read, Seek, SeekFrom};

const MAX_READ_BYTES: usize = 24_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleEncoding {
    PcmInteger,
    Float,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WavFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub encoding: SampleEncoding,
    pub bits_per_sample: u16,
    pub valid_bits_per_sample: u16,
}

pub struct WavReader<R> {
    reader: R,
    format: WavFormat,
    count: u64,
    remaining: u64,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn parse_format(bytes: &[u8], chunk_size: u64) -> io::Result<WavFormat> {
    if bytes.len() < 16 {
        return Err(invalid("WAV format chunk is too short"));
    }
    let original_tag = u16_at(bytes, 0);
    let channels = u16_at(bytes, 2);
    let sample_rate = u32_at(bytes, 4);
    let byte_rate = u32_at(bytes, 8);
    let block_align = u16_at(bytes, 12);
    let bits_per_sample = u16_at(bytes, 14);
    if !(12000..=192000).contains(&sample_rate) {
        return Err(invalid(
            "WAV sample rate must be between 12000 and 192000 Hz",
        ));
    }
    if !(1..=2).contains(&channels) {
        return Err(invalid("WAV must have one or two channels"));
    }

    let (tag, valid_bits_per_sample) = if original_tag == 0xfffe {
        if bytes.len() < 40 {
            return Err(invalid("invalid WAVE_FORMAT_EXTENSIBLE chunk"));
        }
        let extension_size = u16_at(bytes, 16);
        if extension_size < 22 || 18 + u64::from(extension_size) > chunk_size {
            return Err(invalid("invalid WAVE_FORMAT_EXTENSIBLE size"));
        }
        const GUID_SUFFIX: [u8; 12] = [
            0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        ];
        if bytes[28..40] != GUID_SUFFIX {
            return Err(invalid("unsupported WAVE_FORMAT_EXTENSIBLE subformat"));
        }
        let subformat = u32_at(bytes, 24);
        if subformat > u32::from(u16::MAX) {
            return Err(invalid("unsupported WAVE_FORMAT_EXTENSIBLE subformat"));
        }
        (subformat as u16, u16_at(bytes, 18))
    } else {
        (original_tag, bits_per_sample)
    };

    let encoding = match tag {
        1 if matches!(bits_per_sample, 8 | 16 | 24 | 32) => SampleEncoding::PcmInteger,
        3 if bits_per_sample == 32 => SampleEncoding::Float,
        _ => return Err(invalid("unsupported WAV sample encoding")),
    };
    if valid_bits_per_sample == 0 || valid_bits_per_sample > bits_per_sample {
        return Err(invalid("invalid WAV valid-bits field"));
    }
    if encoding == SampleEncoding::Float && valid_bits_per_sample != 32 {
        return Err(invalid("float WAV samples must have 32 valid bits"));
    }

    let bytes_per_sample = bits_per_sample / 8;
    let expected_align = channels
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| invalid("invalid WAV block alignment"))?;
    let expected_byte_rate = sample_rate
        .checked_mul(u32::from(expected_align))
        .ok_or_else(|| invalid("invalid WAV byte rate"))?;
    if block_align != expected_align || byte_rate != expected_byte_rate {
        return Err(invalid("inconsistent WAV byte rate or block alignment"));
    }

    Ok(WavFormat {
        sample_rate,
        channels,
        encoding,
        bits_per_sample,
        valid_bits_per_sample,
    })
}

impl<R: Read + Seek> WavReader<R> {
    pub fn new(mut reader: R) -> io::Result<Self> {
        let file_len = reader.seek(SeekFrom::End(0))?;
        reader.seek(SeekFrom::Start(0))?;
        let mut header = [0u8; 12];
        reader.read_exact(&mut header)?;
        if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
            return Err(invalid("expected RIFF/WAVE audio"));
        }
        let end = u64::from(u32::from_le_bytes(header[4..8].try_into().unwrap()))
            .checked_add(8)
            .ok_or_else(|| invalid("invalid RIFF length"))?;
        if end < 12 || end > file_len {
            return Err(invalid("truncated or invalid RIFF length"));
        }

        let mut position = 12u64;
        let mut format = None;
        let mut data = None;
        while position < end {
            if end - position < 8 {
                return Err(invalid("truncated WAV chunk header"));
            }
            let mut chunk = [0u8; 8];
            reader.read_exact(&mut chunk)?;
            let size = u64::from(u32::from_le_bytes(chunk[4..].try_into().unwrap()));
            let start = position
                .checked_add(8)
                .ok_or_else(|| invalid("invalid WAV chunk position"))?;
            let payload_end = start
                .checked_add(size)
                .ok_or_else(|| invalid("invalid WAV chunk size"))?;
            if payload_end > end {
                return Err(invalid("WAV chunk exceeds RIFF length"));
            }
            // Some recorders omit the final metadata pad from RIFF's length.
            // Padding is only needed to align a following chunk.
            let next = payload_end.saturating_add(size % 2).min(end);
            match &chunk[..4] {
                b"fmt " => {
                    if format.is_some() {
                        return Err(invalid("duplicate WAV format chunk"));
                    }
                    let mut bytes = [0u8; 40];
                    let needed = usize::try_from(size.min(40)).unwrap();
                    reader.read_exact(&mut bytes[..needed])?;
                    format = Some(parse_format(&bytes[..needed], size)?);
                }
                b"data" => {
                    if data.is_some() {
                        return Err(invalid("duplicate WAV data chunk"));
                    }
                    data = Some((start, size));
                }
                _ => {}
            }
            reader.seek(SeekFrom::Start(next))?;
            position = next;
        }

        let format = format.ok_or_else(|| invalid("missing WAV format"))?;
        let (start, data_bytes) = data.ok_or_else(|| invalid("missing WAV data"))?;
        let block_align = u64::from(format.channels * (format.bits_per_sample / 8));
        if data_bytes % block_align != 0 {
            return Err(invalid("WAV data does not contain complete frames"));
        }
        let count = data_bytes / block_align;
        reader.seek(SeekFrom::Start(start))?;
        Ok(Self {
            reader,
            format,
            count,
            remaining: count,
        })
    }

    pub fn format(&self) -> WavFormat {
        self.format
    }

    /// Returns the number of source frames, where a stereo pair is one frame.
    pub fn sample_count(&self) -> u64 {
        self.count
    }

    /// Reads legacy mono 12 kHz signed 16-bit PCM.
    pub fn read_samples(&mut self, maximum: usize) -> io::Result<Vec<i16>> {
        if self.format
            != (WavFormat {
                sample_rate: 12000,
                channels: 1,
                encoding: SampleEncoding::PcmInteger,
                bits_per_sample: 16,
                valid_bits_per_sample: 16,
            })
        {
            return Err(invalid(
                "legacy WAV reads require mono 12000 Hz signed 16-bit PCM",
            ));
        }
        let n = maximum.min(12000).min(self.remaining as usize);
        let mut bytes = vec![0u8; n * 2];
        self.reader.read_exact(&mut bytes)?;
        self.remaining -= n as u64;
        Ok(bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| i16::from_le_bytes(*bytes))
            .collect())
    }

    pub(crate) fn read_float_frames(&mut self, maximum: usize) -> io::Result<Vec<f32>> {
        let bytes_per_sample = usize::from(self.format.bits_per_sample / 8);
        let block_align = usize::from(self.format.channels) * bytes_per_sample;
        let frame_limit = MAX_READ_BYTES / block_align;
        let frames = maximum.min(frame_limit).min(self.remaining as usize);
        let mut bytes = vec![0u8; frames * block_align];
        self.reader.read_exact(&mut bytes)?;
        self.remaining -= frames as u64;

        let mut output = Vec::with_capacity(frames * usize::from(self.format.channels));
        for sample in bytes.chunks_exact(bytes_per_sample) {
            output.push(self.decode_sample(sample)?);
        }
        Ok(output)
    }

    fn decode_sample(&self, bytes: &[u8]) -> io::Result<f32> {
        if self.format.encoding == SampleEncoding::Float {
            let value = f32::from_le_bytes(bytes.try_into().unwrap());
            if !value.is_finite() || !(-1.0..=1.0).contains(&value) {
                return Err(invalid("float WAV sample is nonfinite or outside [-1, 1]"));
            }
            return Ok(value * 32768.0);
        }

        let valid = u32::from(self.format.valid_bits_per_sample);
        let padding = u32::from(self.format.bits_per_sample - self.format.valid_bits_per_sample);
        let value = if self.format.bits_per_sample == 8 {
            i64::from(bytes[0] >> padding) - (1i64 << (valid - 1))
        } else {
            let container = match self.format.bits_per_sample {
                16 => i64::from(i16::from_le_bytes(bytes.try_into().unwrap())),
                24 => {
                    let raw = i64::from(bytes[0])
                        | (i64::from(bytes[1]) << 8)
                        | (i64::from(bytes[2]) << 16);
                    if raw & 0x80_0000 != 0 {
                        raw | !0xff_ffff
                    } else {
                        raw
                    }
                }
                32 => i64::from(i32::from_le_bytes(bytes.try_into().unwrap())),
                _ => unreachable!(),
            };
            container >> padding
        };
        let scale = 32768.0f64 / (1u64 << (valid - 1)) as f64;
        Ok((value as f64 * scale) as f32)
    }
}
