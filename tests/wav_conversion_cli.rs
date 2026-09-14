use serde_json::Value;
use std::process::Command;

// Independent bandlimited fixture construction by Fourier interpolation.
// The production converter uses time-domain sinc filters. Avoid sample-and-hold
// fixture generation: its imaging/time jitter can destroy a marginal decode.
// No additional audio files are stored in the repository.
fn recording(
    mode: &str,
    weak: bool,
    rate: u32,
    float: bool,
    stereo: bool,
    prefix: usize,
) -> Vec<u8> {
    let original = std::fs::read(format!(
        "tests/fixtures/{mode}-{}.wav",
        if weak { "weak" } else { "clean" }
    ))
    .unwrap();
    let pcm: Vec<i16> = original[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    // Round up the generated source duration: a fractional source frame
    // otherwise shortens FT4 below its exact required decoding window.
    let frames = (pcm.len() as u64 * rate as u64).div_ceil(12000);
    use rustfft::{FftPlanner, num_complex::Complex};
    let mut planner = FftPlanner::<f64>::new();
    let mut spectrum: Vec<_> = pcm
        .iter()
        .map(|&v| Complex::new(f64::from(v), 0.0))
        .collect();
    planner.plan_fft_forward(pcm.len()).process(&mut spectrum);
    let mut upsampled = vec![Complex::new(0.0, 0.0); frames as usize];
    let half = pcm.len() / 2;
    upsampled[..half].copy_from_slice(&spectrum[..half]);
    // Split the real input Nyquist bin between its positive and negative bins.
    upsampled[half] = spectrum[half] * 0.5;
    upsampled[frames as usize - half] = spectrum[half] * 0.5;
    upsampled[frames as usize - half + 1..].copy_from_slice(&spectrum[half + 1..]);
    planner
        .plan_fft_inverse(frames as usize)
        .process(&mut upsampled);
    let mut data = Vec::new();
    for i in 0..frames as usize + prefix {
        let sample = if i < prefix {
            0.0
        } else {
            (upsampled[i - prefix].re / pcm.len() as f64) as f32
        };
        for channel in 0..if stereo { 2 } else { 1 } {
            let value = if stereo && channel == 0 { 0.0 } else { sample };
            if float {
                data.extend((value / 32768.0).to_le_bytes());
            } else {
                data.extend_from_slice(&((value as i32) << 8).to_le_bytes()[..3]);
            }
        }
    }
    let channels = if stereo { 2u16 } else { 1 };
    let bits = if float { 32u16 } else { 24 };
    let block = channels * (bits / 8);
    let mut wav = Vec::new();
    wav.extend(b"RIFF");
    wav.extend((36 + data.len() as u32 + (data.len() % 2) as u32).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend((if float { 3u16 } else { 1 }).to_le_bytes());
    wav.extend(channels.to_le_bytes());
    wav.extend(rate.to_le_bytes());
    wav.extend((rate * u32::from(block)).to_le_bytes());
    wav.extend(block.to_le_bytes());
    wav.extend(bits.to_le_bytes());
    wav.extend(b"data");
    wav.extend((data.len() as u32).to_le_bytes());
    wav.extend(&data);
    if data.len() % 2 != 0 {
        wav.push(0);
    }
    wav
}

#[test]
fn converted_clean_and_weak_recordings_decode_with_preserved_timing() {
    for mode in ["ft4", "ft8"] {
        for (rate, float, stereo, weak) in [(48000, false, true, false), (44100, true, false, true)]
        {
            let prefix = rate as usize * 35 / 100;
            let path = std::env::temp_dir().join(format!(
                "ftdecode-conversion-{}-{mode}-{rate}.wav",
                std::process::id()
            ));
            std::fs::write(&path, recording(mode, weak, rate, float, stereo, prefix)).unwrap();
            let result = Command::new(env!("CARGO_BIN_EXE_ftdecode"))
                .args([
                    "decode",
                    path.to_str().unwrap(),
                    "--mode",
                    mode,
                    "--output",
                    "jsonl",
                    "--channel",
                    if stereo { "right" } else { "left" },
                    "--slot-offset-seconds",
                    "0.35",
                ])
                .output()
                .unwrap();
            std::fs::remove_file(&path).unwrap();
            assert!(
                result.status.success(),
                "{mode}/{rate}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let text = String::from_utf8(result.stdout).unwrap();
            let events: Vec<Value> = text
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            let decodes: Vec<_> = events.iter().filter(|v| v["type"] == "decode").collect();
            assert_eq!(decodes.len(), 1, "{mode}/{rate}: {text}");
            assert_eq!(decodes[0]["message"], "CQ K1ABC FN42");
            assert!((decodes[0]["frequency_hz"].as_f64().unwrap() - 1500.0).abs() < 2.0);
            assert!(decodes[0]["dt_seconds"].as_f64().unwrap().abs() < 0.1);
            assert_eq!(decodes[0]["window_start_sample"], "4200");
        }
    }
}

#[test]
fn source_frame_offsets_and_seconds_agree_and_partial_tails_stay_skipped() {
    let path = std::env::temp_dir().join(format!("ftdecode-offset-{}.wav", std::process::id()));
    let prefix = 16800;
    std::fs::write(&path, recording("ft4", false, 48000, false, false, prefix)).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ftdecode"))
            .args([
                "decode",
                path.to_str().unwrap(),
                "--mode",
                "ft4",
                "--output",
                "jsonl",
            ])
            .args(args)
            .output()
            .unwrap()
    };
    let samples = run(&["--slot-offset-samples", "16800"]);
    let seconds = run(&["--slot-offset-seconds", "0.35"]);
    assert!(
        samples.status.success(),
        "{}",
        String::from_utf8_lossy(&samples.stderr)
    );
    assert!(seconds.status.success());
    assert_eq!(samples.stdout, seconds.stdout);
    for args in [
        vec!["--slot-offset-seconds", "NaN"],
        vec!["--slot-offset-seconds", "-1"],
        vec!["--slot-offset-seconds", "1", "--slot-offset-samples", "0"],
        vec!["--slot-offset-seconds", "1", "--start-utc-ns", "0"],
    ] {
        assert!(!run(&args).status.success(), "{args:?}");
    }
    // Exactly one source frame short of FT4's 72576-sample requirement.
    let mut wav = recording("ft4", false, 48000, false, false, 0);
    wav.truncate(44 + (72576 * 4 - 1) * 3);
    let bytes = (wav.len() - 44) as u32;
    wav[40..44].copy_from_slice(&bytes.to_le_bytes());
    if bytes % 2 == 1 {
        wav.push(0);
    }
    let len = wav.len() as u32;
    wav[4..8].copy_from_slice(&(len - 8).to_le_bytes());
    std::fs::write(&path, wav).unwrap();
    let short = run(&[]);
    std::fs::remove_file(&path).unwrap();
    assert!(
        short.status.success(),
        "{}",
        String::from_utf8_lossy(&short.stderr)
    );
    let text = String::from_utf8(short.stdout).unwrap();
    assert!(text.contains("incomplete tail"), "{text}");
    assert!(!text.contains("\"status\":\"completed\""), "{text}");
}
