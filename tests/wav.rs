// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{
    audio_input::{Channel, ConvertedWav},
    wav::{SampleEncoding, WavReader},
};
use std::io::Cursor;

fn wav(channels: u16, rate: u32, bits: u16, samples: &[i16]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend((36 + samples.len() as u32 * 2).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(channels.to_le_bytes());
    b.extend(rate.to_le_bytes());
    b.extend((rate * u32::from(channels) * u32::from(bits) / 8).to_le_bytes());
    b.extend((channels * bits / 8).to_le_bytes());
    b.extend(bits.to_le_bytes());
    b.extend(b"data");
    b.extend((samples.len() as u32 * 2).to_le_bytes());
    for s in samples {
        b.extend(s.to_le_bytes());
    }
    b
}

fn raw_wav(format: &[u8], data: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    let riff_size = 4 + 8 + format.len() + (format.len() & 1) + 8 + data.len();
    b.extend(b"RIFF");
    b.extend((riff_size as u32).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend((format.len() as u32).to_le_bytes());
    b.extend(format);
    if format.len() & 1 != 0 {
        b.push(0);
    }
    b.extend(b"data");
    b.extend((data.len() as u32).to_le_bytes());
    b.extend(data);
    b
}

fn format(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
    let bytes = bits / 8;
    let mut f = Vec::new();
    f.extend(tag.to_le_bytes());
    f.extend(channels.to_le_bytes());
    f.extend(rate.to_le_bytes());
    f.extend((rate * u32::from(channels * bytes)).to_le_bytes());
    f.extend((channels * bytes).to_le_bytes());
    f.extend(bits.to_le_bytes());
    f
}

fn extensible_format(
    subformat: u32,
    channels: u16,
    rate: u32,
    bits: u16,
    valid_bits: u16,
) -> Vec<u8> {
    let mut f = format(0xfffe, channels, rate, bits);
    f.extend(22u16.to_le_bytes());
    f.extend(valid_bits.to_le_bytes());
    f.extend(0u32.to_le_bytes());
    f.extend(subformat.to_le_bytes());
    f.extend([
        0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
    ]);
    f
}

fn converted(bytes: Vec<u8>, channel: Channel) -> std::io::Result<Vec<f32>> {
    let wav = WavReader::new(Cursor::new(bytes))?;
    let mut wav = ConvertedWav::new(wav, channel)?;
    let mut samples = Vec::new();
    loop {
        let block = wav.read_samples(97)?;
        if block.is_empty() {
            break;
        }
        samples.extend(block);
    }
    Ok(samples)
}
#[test]
fn pcm_is_read_in_bounded_blocks_with_exact_sample_count() {
    let mut reader = WavReader::new(Cursor::new(wav(1, 12000, 16, &[-32768, 7, 32767]))).unwrap();
    assert_eq!(reader.sample_count(), 3);
    assert_eq!(reader.read_samples(2).unwrap(), vec![-32768, 7]);
    assert_eq!(reader.read_samples(2).unwrap(), vec![32767]);
    assert!(reader.read_samples(2).unwrap().is_empty());
}
#[test]
fn legacy_i16_reads_reject_converted_formats_and_truncation_is_rejected() {
    for b in [
        wav(2, 12000, 16, &[1, 2]),
        wav(1, 48000, 16, &[1]),
        wav(1, 12000, 8, &[1]),
    ] {
        let mut reader = WavReader::new(Cursor::new(b)).unwrap();
        assert!(reader.read_samples(1).is_err());
    }
    let mut truncated = wav(1, 12000, 16, &[1, 2]);
    truncated.pop();
    assert!(WavReader::new(Cursor::new(truncated)).is_err());
}
#[test]
fn odd_sized_unknown_chunks_are_skipped_with_padding() {
    let mut b = wav(1, 12000, 16, &[12]);
    let extra = [b'J', b'U', b'N', b'K', 1, 0, 0, 0, 99, 0];
    b.splice(12..12, extra);
    let len = b.len() as u32 - 8;
    b[4..8].copy_from_slice(&len.to_le_bytes());
    let mut reader = WavReader::new(Cursor::new(b)).unwrap();
    assert_eq!(reader.read_samples(12000).unwrap(), vec![12]);
}

#[test]
fn final_metadata_padding_may_be_outside_declared_riff_size() {
    // Native WSJT-X sample WAVs exclude the final LIST pad from RIFF length.
    for pad in [false, true] {
        let mut b = wav(1, 12000, 16, &[21]);
        b.extend(b"LIST");
        b.extend(1u32.to_le_bytes());
        b.push(42);
        let len = b.len() as u32 - 8;
        b[4..8].copy_from_slice(&len.to_le_bytes());
        if pad {
            b.push(0);
        }
        let mut reader = WavReader::new(Cursor::new(b)).unwrap();
        assert_eq!(reader.read_samples(12000).unwrap(), vec![21]);
    }
}

#[test]
fn common_pcm_widths_convert_to_pcm_scale_without_i16_quantization() {
    let cases = [
        (8, vec![0, 128, 255], vec![-32768.0, 0.0, 32512.0]),
        (
            16,
            [-32768i16, 0, 32767]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect(),
            vec![-32768.0, 0.0, 32767.0],
        ),
        (
            24,
            vec![0x00, 0x00, 0x80, 0, 0, 0, 0xff, 0xff, 0x7f],
            vec![-32768.0, 0.0, 32767.996],
        ),
        (
            32,
            [i32::MIN, 0, i32::MAX]
                .into_iter()
                .flat_map(i32::to_le_bytes)
                .collect(),
            vec![-32768.0, 0.0, 32768.0],
        ),
    ];
    for (bits, data, expected) in cases {
        let bytes = raw_wav(&format(1, 1, 12000, bits), &data);
        let actual = converted(bytes, Channel::Left).unwrap();
        assert_eq!(actual.len(), expected.len(), "{bits}-bit length");
        for (actual, expected) in actual.iter().zip(expected) {
            assert!(
                (actual - expected).abs() < 0.01,
                "{bits}-bit: {actual} != {expected}"
            );
        }
    }
}

#[test]
fn float32_is_pcm_scaled_and_invalid_values_are_rejected_when_read() {
    let valid: Vec<u8> = [-1.0f32, -0.25, 0.5, 1.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect();
    assert_eq!(
        converted(raw_wav(&format(3, 1, 12000, 32), &valid), Channel::Left).unwrap(),
        vec![-32768.0, -8192.0, 16384.0, 32768.0]
    );
    for invalid in [f32::NAN, f32::INFINITY, -1.001, 1.001] {
        let bytes = raw_wav(&format(3, 1, 12000, 32), &invalid.to_le_bytes());
        let err = converted(bytes, Channel::Left).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}

#[test]
fn stereo_selection_uses_frames_and_mix_averages_in_float() {
    let samples = [-32768i16, 32767, 1001, -1000];
    let data: Vec<u8> = samples.into_iter().flat_map(i16::to_le_bytes).collect();
    let bytes = raw_wav(&format(1, 2, 12000, 16), &data);
    assert_eq!(
        converted(bytes.clone(), Channel::Left).unwrap(),
        vec![-32768.0, 1001.0]
    );
    assert_eq!(
        converted(bytes.clone(), Channel::Right).unwrap(),
        vec![32767.0, -1000.0]
    );
    assert_eq!(converted(bytes, Channel::Mix).unwrap(), vec![-0.5, 0.5]);
}

#[test]
fn extensible_pcm_uses_valid_bits_and_standard_subformat_guid() {
    // 20 valid signed bits are stored left-aligned in each 24-bit container.
    let data = [[0x00, 0x00, 0x80], [0xf0, 0xff, 0xff], [0xf0, 0xff, 0x7f]].concat();
    let bytes = raw_wav(&extensible_format(1, 1, 12000, 24, 20), &data);
    assert_eq!(
        converted(bytes, Channel::Left).unwrap(),
        vec![-32768.0, -0.0625, 32768.0 - 0.0625]
    );

    let mut invalid_guid = extensible_format(1, 1, 12000, 24, 20);
    invalid_guid[39] ^= 1;
    assert!(WavReader::new(Cursor::new(raw_wav(&invalid_guid, &data))).is_err());
}

#[test]
fn extensible_format_rejects_cbsize_larger_than_its_chunk() {
    let mut f = extensible_format(1, 1, 12000, 24, 20);
    f[16..18].copy_from_slice(&23u16.to_le_bytes());
    assert!(WavReader::new(Cursor::new(raw_wav(&f, &[]))).is_err());

    f.push(0);
    assert!(WavReader::new(Cursor::new(raw_wav(&f, &[]))).is_ok());
}

#[test]
fn metadata_describes_source_format() {
    let reader = WavReader::new(Cursor::new(raw_wav(
        &extensible_format(3, 2, 44100, 32, 32),
        &[0; 8],
    )))
    .unwrap();
    let f = reader.format();
    assert_eq!(f.sample_rate, 44100);
    assert_eq!(f.channels, 2);
    assert_eq!(f.bits_per_sample, 32);
    assert_eq!(f.valid_bits_per_sample, 32);
    assert_eq!(f.encoding, SampleEncoding::Float);
    assert_eq!(reader.sample_count(), 1);
}

#[test]
fn malformed_and_unsupported_formats_are_rejected() {
    for f in [
        format(1, 1, 11999, 16),
        format(1, 1, 192001, 16),
        format(1, 3, 12000, 16),
        format(1, 1, 12000, 12),
        format(3, 1, 12000, 64),
        format(6, 1, 12000, 16),
        extensible_format(1, 1, 12000, 24, 0),
        extensible_format(1, 1, 12000, 24, 25),
    ] {
        assert!(WavReader::new(Cursor::new(raw_wav(&f, &[]))).is_err());
    }
    for rate in [12000, 192000] {
        assert!(WavReader::new(Cursor::new(raw_wav(&format(1, 1, rate, 16), &[]))).is_ok());
    }

    let f = format(1, 2, 12000, 16);
    assert!(WavReader::new(Cursor::new(raw_wav(&f, &[0, 0]))).is_err());
}

#[test]
fn mono_rejects_a_right_channel_request() {
    let wav = WavReader::new(Cursor::new(wav(1, 12000, 16, &[1]))).unwrap();
    assert!(ConvertedWav::new(wav, Channel::Right).is_err());
}
