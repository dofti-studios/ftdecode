// SPDX-License-Identifier: GPL-3.0-or-later

#[path = "common/dsp.rs"]
mod dsp;

use ftdecode::downsample::{DownsampleError, Ft4Downsampler, Ft8Downsampler};
use ftdecode::fft::Complex32;
use serde_json::Value;

const REL_TOLERANCE: f64 = 2.0e-5;
const ABS_TOLERANCE: f64 = 1.0e-3;

fn assert_native_cases(kind: &str) {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/dsp-vectors.json")).unwrap();
    for case in fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["kind"] == kind)
    {
        let input_len = case["length"].as_u64().unwrap() as usize;
        let input = dsp::samples(&case["recipe"], input_len);
        let output_len = case["output_length"].as_u64().unwrap() as usize;
        let mut actual = vec![Complex32::new(0.0, 0.0); output_len];
        let frequency = case["frequency_hz"].as_f64().unwrap() as f32;
        match kind {
            "ft8" => {
                let mut downsampler = Ft8Downsampler::new();
                downsampler.prepare(&input).unwrap();
                downsampler.extract(frequency, &mut actual).unwrap();
            }
            "ft4" => {
                let mut downsampler = Ft4Downsampler::new();
                downsampler.prepare(&input).unwrap();
                downsampler.extract(frequency, &mut actual).unwrap();
            }
            _ => unreachable!(),
        }
        let expected = case["values"].as_array().unwrap();
        assert_eq!(actual.len(), expected.len());
        let mut squared_error = 0.0;
        let mut squared_reference = 0.0;
        let mut peak_error = 0.0_f64;
        let mut peak_reference = 0.0_f64;
        for (actual, expected) in actual.iter().zip(expected) {
            let re = expected[0].as_f64().unwrap();
            let im = expected[1].as_f64().unwrap();
            let error = ((actual.re as f64 - re).powi(2) + (actual.im as f64 - im).powi(2)).sqrt();
            squared_error += error * error;
            squared_reference += re * re + im * im;
            peak_error = peak_error.max(error);
            peak_reference = peak_reference.max((re * re + im * im).sqrt());
        }
        let rms_error = (squared_error / actual.len() as f64).sqrt();
        let rms_reference = (squared_reference / actual.len() as f64).sqrt();
        let relative_rms = rms_error / rms_reference.max(f64::MIN_POSITIVE);
        println!(
            "{kind} {:?} at {frequency} Hz: relative RMS {relative_rms:e}, peak {peak_error:e}",
            case["recipe"]
        );
        assert!(
            rms_error <= ABS_TOLERANCE + REL_TOLERANCE * rms_reference
                && peak_error <= ABS_TOLERANCE + REL_TOLERANCE * peak_reference,
            "{kind} {:?} at {frequency} Hz: relative RMS {relative_rms:e}, peak {peak_error:e}",
            case["recipe"]
        );
    }
}

#[test]
fn ft8_matches_complete_native_outputs() {
    assert_native_cases("ft8");
}

#[test]
fn ft4_matches_complete_native_outputs() {
    assert_native_cases("ft4");
}

#[test]
fn extraction_requires_a_successful_prepare_and_reset_invalidates_it() {
    let mut downsampler = Ft8Downsampler::new();
    let mut output = vec![Complex32::new(0.0, 0.0); 3200];
    assert_eq!(
        downsampler.extract(1500.0, &mut output),
        Err(DownsampleError::NotPrepared)
    );
    downsampler.prepare(&vec![0.0; 180_000]).unwrap();
    downsampler.reset();
    assert_eq!(
        downsampler.extract(1500.0, &mut output),
        Err(DownsampleError::NotPrepared)
    );
}

#[test]
fn failed_prepare_invalidates_the_previous_spectrum() {
    let mut downsampler = Ft4Downsampler::new();
    downsampler.prepare(&vec![0.0; 72_576]).unwrap();
    assert!(matches!(
        downsampler.prepare(&vec![0.0; 72_575]),
        Err(DownsampleError::InvalidInputLength { .. })
    ));
    let mut output = vec![Complex32::new(0.0, 0.0); 4032];
    assert_eq!(
        downsampler.extract(1500.0, &mut output),
        Err(DownsampleError::NotPrepared)
    );
}

#[test]
fn prepare_rejects_non_finite_and_out_of_pcm_range_samples() {
    for invalid in [f32::NAN, f32::INFINITY, -32_769.0, 32_769.0] {
        let mut input = vec![0.0; 180_000];
        input[91] = invalid;
        assert!(matches!(
            Ft8Downsampler::new().prepare(&input),
            Err(DownsampleError::InvalidSample { index: 91 })
        ));
    }
}

#[test]
fn extract_validates_frequency_and_output_shape() {
    let mut downsampler = Ft4Downsampler::new();
    downsampler.prepare(&vec![0.0; 72_576]).unwrap();
    for frequency in [f32::NAN, f32::INFINITY, -0.01, 6000.01] {
        assert!(matches!(
            downsampler.extract(frequency, &mut vec![Complex32::new(0.0, 0.0); 4032]),
            Err(DownsampleError::InvalidFrequency)
        ));
    }
    assert!(matches!(
        downsampler.extract(1500.0, &mut vec![Complex32::new(0.0, 0.0); 4031]),
        Err(DownsampleError::InvalidOutputLength { .. })
    ));
}

#[test]
fn repeated_extracts_are_stable_and_new_audio_replaces_the_cache() {
    let mut downsampler = Ft8Downsampler::new();
    let mut input = vec![0.0; 180_000];
    input[0] = 32767.0;
    downsampler.prepare(&input).unwrap();
    let mut first = vec![Complex32::new(0.0, 0.0); 3200];
    let mut repeated = first.clone();
    downsampler.extract(1500.0, &mut first).unwrap();
    downsampler.extract(1500.0, &mut repeated).unwrap();
    assert_eq!(first, repeated);
    downsampler.prepare(&vec![0.0; 180_000]).unwrap();
    downsampler.extract(1500.0, &mut repeated).unwrap();
    assert!(
        repeated
            .iter()
            .all(|sample| *sample == Complex32::new(0.0, 0.0))
    );
    assert!(
        first
            .iter()
            .any(|sample| *sample != Complex32::new(0.0, 0.0))
    );
}

#[test]
fn prepared_spectra_are_isolated_per_instance() {
    let mut impulse = Ft4Downsampler::new();
    let mut silence = Ft4Downsampler::new();
    let mut input = vec![0.0; 72_576];
    input[0] = 32767.0;
    impulse.prepare(&input).unwrap();
    silence.prepare(&vec![0.0; 72_576]).unwrap();
    let mut a = vec![Complex32::new(0.0, 0.0); 4032];
    let mut b = a.clone();
    impulse.extract(1500.0, &mut a).unwrap();
    silence.extract(1500.0, &mut b).unwrap();
    assert!(a.iter().any(|sample| *sample != Complex32::new(0.0, 0.0)));
    assert!(b.iter().all(|sample| *sample == Complex32::new(0.0, 0.0)));
}
