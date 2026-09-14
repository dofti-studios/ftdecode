// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::fft::{Complex32, ComplexFft, Direction, RealForward};
use serde_json::Value;
#[path = "common/dsp.rs"]
mod dsp;

#[test]
fn fft_sign_scale_and_sampled_bins_match_native_at_decoder_sizes() {
    let data: Value = serde_json::from_str(include_str!("fixtures/dsp-vectors.json")).unwrap();
    for case in data["cases"].as_array().unwrap() {
        let kind = case["kind"].as_str().unwrap();
        if !kind.ends_with("forward") && !kind.ends_with("inverse") {
            continue;
        }
        let n = case["length"].as_u64().unwrap() as usize;
        let actual = if kind == "real-forward" {
            let mut input = dsp::samples(&case["recipe"], n);
            let mut output = vec![Complex32::default(); n / 2 + 1];
            RealForward::new(n)
                .unwrap()
                .transform(&mut input, &mut output)
                .unwrap();
            output
        } else {
            let input = dsp::samples(&case["recipe"], 2 * n);
            let mut values: Vec<_> = input
                .as_chunks::<2>()
                .0
                .iter()
                .map(|x| Complex32::new(x[0], x[1]))
                .collect();
            let direction = if kind == "complex-forward" {
                Direction::Forward
            } else {
                Direction::Inverse
            };
            ComplexFft::new(n, direction)
                .unwrap()
                .transform(&mut values)
                .unwrap();
            values
        };
        assert_eq!(
            actual.len(),
            case["output_length"].as_u64().unwrap() as usize
        );
        let mut error_energy = 0.0f64;
        let mut reference_energy = 0.0f64;
        let mut peak_error = 0.0f64;
        let mut peak_reference = 0.0f64;
        for (index, value) in case["indices"]
            .as_array()
            .unwrap()
            .iter()
            .zip(case["values"].as_array().unwrap())
        {
            let expected = Complex32::new(
                value[0].as_f64().unwrap() as f32,
                value[1].as_f64().unwrap() as f32,
            );
            let error = (actual[index.as_u64().unwrap() as usize] - expected).norm() as f64;
            let magnitude = expected.norm() as f64;
            error_energy += error * error;
            reference_energy += magnitude * magnitude;
            peak_error = peak_error.max(error);
            peak_reference = peak_reference.max(magnitude);
        }
        let relative_rms = (error_energy / reference_energy).sqrt();
        let relative_peak = peak_error / peak_reference;
        eprintln!("{kind} {n}: RMS={relative_rms:.3e}, peak={relative_peak:.3e}");
        assert!(
            relative_rms < 2e-6,
            "{kind} {n}: relative RMS {relative_rms}"
        );
        assert!(
            relative_peak < 2e-6,
            "{kind} {n}: relative peak {relative_peak}"
        );
    }
}
