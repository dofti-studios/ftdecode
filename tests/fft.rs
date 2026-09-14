// SPDX-License-Identifier: GPL-3.0-or-later

use ftdecode::fft::{Complex32, ComplexFft, Direction, RealForward};

const EPSILON: f32 = 1.0e-4;

fn assert_complex_close(actual: Complex32, expected: Complex32) {
    assert!(
        (actual - expected).norm() <= EPSILON,
        "expected {expected:?}, got {actual:?}"
    );
}

#[test]
fn complex_forward_uses_negative_exponent() {
    let mut fft = ComplexFft::new(4, Direction::Forward).unwrap();
    let mut values = [
        Complex32::new(0.0, 0.0),
        Complex32::new(1.0, 0.0),
        Complex32::new(0.0, 0.0),
        Complex32::new(0.0, 0.0),
    ];

    fft.transform(&mut values).unwrap();

    let expected = [
        Complex32::new(1.0, 0.0),
        Complex32::new(0.0, -1.0),
        Complex32::new(-1.0, 0.0),
        Complex32::new(0.0, 1.0),
    ];
    for (actual, expected) in values.into_iter().zip(expected) {
        assert_complex_close(actual, expected);
    }
}

#[test]
fn complex_inverse_is_unnormalized() {
    let original = [
        Complex32::new(1.0, -0.5),
        Complex32::new(-2.0, 0.25),
        Complex32::new(0.75, 3.0),
        Complex32::new(4.0, -1.5),
    ];
    let mut values = original;
    let mut forward = ComplexFft::new(values.len(), Direction::Forward).unwrap();
    let mut inverse = ComplexFft::new(values.len(), Direction::Inverse).unwrap();

    forward.transform(&mut values).unwrap();
    inverse.transform(&mut values).unwrap();

    for (actual, original) in values.into_iter().zip(original) {
        assert_complex_close(actual, original * 4.0);
    }
}

#[test]
fn real_forward_returns_the_nonnegative_half_spectrum() {
    let mut fft = RealForward::new(8).unwrap();
    let mut input = [1.0; 8];
    let mut output = [Complex32::new(99.0, 99.0); 5];

    fft.transform(&mut input, &mut output).unwrap();

    assert_complex_close(output[0], Complex32::new(8.0, 0.0));
    for value in output[1..].iter().copied() {
        assert_complex_close(value, Complex32::new(0.0, 0.0));
    }
}

#[test]
fn real_forward_places_a_cosine_in_its_positive_frequency_bin() {
    let mut fft = RealForward::new(32).unwrap();
    let mut input = std::array::from_fn::<_, 32, _>(|sample| {
        (2.0 * std::f32::consts::PI * 3.0 * sample as f32 / 32.0).cos()
    });
    let mut output = [Complex32::new(0.0, 0.0); 17];

    fft.transform(&mut input, &mut output).unwrap();

    assert_complex_close(output[3], Complex32::new(16.0, 0.0));
    for (bin, value) in output.into_iter().enumerate().filter(|(bin, _)| *bin != 3) {
        assert!(
            value.norm() <= 2.0e-4,
            "unexpected energy in bin {bin}: {value:?}"
        );
    }
}

#[test]
fn constructors_reject_zero_length() {
    assert!(ComplexFft::new(0, Direction::Forward).is_err());
    assert!(ComplexFft::new(0, Direction::Inverse).is_err());
    assert!(RealForward::new(0).is_err());
}

#[test]
fn transforms_reject_incorrect_buffer_shapes() {
    let mut complex = ComplexFft::new(4, Direction::Forward).unwrap();
    assert!(
        complex
            .transform(&mut [Complex32::new(0.0, 0.0); 3])
            .is_err()
    );
    assert!(
        complex
            .transform(&mut [Complex32::new(0.0, 0.0); 5])
            .is_err()
    );

    let mut real = RealForward::new(8).unwrap();
    let mut correct_input = [0.0; 8];
    let mut short_input = [0.0; 7];
    let mut long_input = [0.0; 9];
    let mut correct_output = [Complex32::new(0.0, 0.0); 5];
    let mut short_output = [Complex32::new(0.0, 0.0); 4];
    let mut long_output = [Complex32::new(0.0, 0.0); 6];
    assert!(
        real.transform(&mut short_input, &mut correct_output)
            .is_err()
    );
    assert!(
        real.transform(&mut long_input, &mut correct_output)
            .is_err()
    );
    assert!(
        real.transform(&mut correct_input, &mut short_output)
            .is_err()
    );
    assert!(
        real.transform(&mut correct_input, &mut long_output)
            .is_err()
    );
}

#[test]
fn plans_can_be_reused_at_decoder_transform_sizes() {
    for len in [192_000, 72_576, 3_200, 4_032, 3_840, 2_304, 32, 576] {
        let mut fft = ComplexFft::new(len, Direction::Forward).unwrap();
        let mut values = vec![Complex32::new(0.0, 0.0); len];
        values[0] = Complex32::new(1.0, 0.0);
        fft.transform(&mut values).unwrap();
        assert_complex_close(values[0], Complex32::new(1.0, 0.0));

        values.fill(Complex32::new(0.0, 0.0));
        values[1] = Complex32::new(2.0, 0.0);
        fft.transform(&mut values).unwrap();
        assert_complex_close(values[0], Complex32::new(2.0, 0.0));
    }
}

#[test]
fn real_fft_matches_direct_dft_for_small_even_odd_and_prime_lengths() {
    for len in [1, 2, 3, 4, 5, 6, 7, 8, 11, 16, 30, 32] {
        let input: Vec<_> = (0..len).map(|i| ((i * 17 + 3) % 13) as f32 - 6.0).collect();
        let mut fft = RealForward::new(len).unwrap();
        let mut actual = vec![Complex32::default(); len / 2 + 1];
        fft.transform(&mut input.clone(), &mut actual).unwrap();
        for (k, actual) in actual.iter().enumerate() {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, &x) in input.iter().enumerate() {
                let angle = -std::f64::consts::TAU * (k * i) as f64 / len as f64;
                re += f64::from(x) * angle.cos();
                im += f64::from(x) * angle.sin();
            }
            assert!((f64::from(actual.re) - re).abs() < 1e-4, "N={len}, bin={k}");
            assert!((f64::from(actual.im) - im).abs() < 1e-4, "N={len}, bin={k}");
        }
    }
}

#[test]
fn real_fft_matches_full_complex_spectrum_at_production_sizes_and_reuses_buffers() {
    for len in [32, 576, 2304, 3200, 3840, 4032, 72576, 180000, 192000] {
        let mut real = RealForward::new(len).unwrap();
        let mut complex = ComplexFft::new(len, Direction::Forward).unwrap();
        let mut output = vec![Complex32::new(f32::NAN, f32::NAN); len / 2 + 1];
        let mut seed = 42u32;
        for round in 0..3 {
            let mut input: Vec<_> = (0..len)
                .map(|i| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    match round {
                        0 => (seed >> 8) as f32 / 16777216.0 - 0.5,
                        1 => {
                            if i % 2 == 0 {
                                0.75
                            } else {
                                -0.25
                            }
                        }
                        _ => 0.0,
                    }
                })
                .collect();
            let mut expected: Vec<_> = input.iter().map(|&x| Complex32::new(x, 0.0)).collect();
            complex.transform(&mut expected).unwrap();
            real.transform(&mut input, &mut output).unwrap();
            let error: f64 = output
                .iter()
                .zip(&expected)
                .map(|(a, b)| f64::from((*a - *b).norm_sqr()))
                .sum();
            let energy: f64 = expected[..output.len()]
                .iter()
                .map(|x| f64::from(x.norm_sqr()))
                .sum();
            assert!(
                error.sqrt() <= 2e-6 * energy.sqrt().max(1.0),
                "N={len}, round={round}: {error}/{energy}"
            );
            assert_eq!(output[0].im, 0.0);
            assert_eq!(output[len / 2].im, 0.0);
        }
    }
}
