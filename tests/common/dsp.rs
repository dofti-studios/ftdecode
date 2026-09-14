// SPDX-License-Identifier: GPL-3.0-or-later
use serde_json::Value;

/// Input generation only, shared with the independent native probe recipe.
pub fn samples(recipe: &Value, n: usize) -> Vec<f32> {
    match recipe["kind"].as_str().unwrap() {
        "silence" => vec![0.0; n],
        "impulse" => {
            let mut values = vec![0.0; n];
            values[recipe["index"].as_u64().unwrap() as usize] =
                recipe["amplitude"].as_f64().unwrap() as f32;
            values
        }
        "noise" => {
            let mut state = recipe["seed"].as_u64().unwrap() as u32;
            (0..n)
                .map(|_| {
                    state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                    ((state >> 16) as i32 - 32768) as f32
                })
                .collect()
        }
        "tone" => {
            let amplitude = recipe["amplitude"].as_f64().unwrap();
            let hz = recipe["hz"].as_f64().unwrap();
            let phase = recipe["phase"].as_f64().unwrap();
            (0..n)
                .map(|i| {
                    (amplitude
                        * (2.0 * std::f64::consts::PI * hz * i as f64 / 12000.0 + phase).cos())
                        as f32
                })
                .collect()
        }
        kind => panic!("unknown DSP recipe: {kind}"),
    }
}
