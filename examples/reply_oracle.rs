//! Emit interoperability facts through the public message API.
use ftdecode::{
    engine::{DecodedSignal, Mode},
    message::MessageDecoder,
    message_encode,
    stream::signal_json,
};
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut decoder = MessageDecoder::default();
    for call in [
        "K1ABC",
        "W9XYZ",
        "G4ABC/P",
        "PJ4/K1ABC",
        "YW18FIFA",
        "PA9XYZ",
    ] {
        decoder.remember_call(call).unwrap();
    }
    if args.first().is_some_and(|s| s == "--tables") {
        for (name, start, width, max, primary, subtype) in [
            ("section", 64, 7, 127, 0, 3),
            ("multiplier", 61, 13, 191, 3, 0),
        ] {
            let fixtures: serde_json::Value = serde_json::from_str(include_str!(
                "../tests/fixtures/receive-message-vectors.json"
            ))
            .unwrap();
            let label = if name == "section" {
                "K1ABC W9XYZ 1A AB"
            } else {
                "K1ABC W9XYZ 599 7999"
            };
            let fixture = fixtures["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == label)
                .unwrap();
            let base: [bool; 77] = fixture["messages"][0]["bits"]
                .as_str()
                .unwrap()
                .bytes()
                .map(|b| b == b'1')
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            for index in 0..=max {
                let mut bits = base;
                let value = if name == "section" {
                    index
                } else {
                    8001 + index
                };
                for i in 0..width {
                    bits[start + i] = (value >> (width - 1 - i)) & 1 != 0;
                }
                if let Ok(message) = decoder.unpack(&bits) {
                    println!(
                        "{}",
                        serde_json::json!({"table":name,"value":value,"text":message.text,"primary":primary,"subtype":subtype})
                    );
                }
            }
        }
        return;
    }
    if args.first().is_some_and(|s| s == "--receive") {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/receive-message-vectors.json"
        ))
        .unwrap();
        for session in fixtures["sessions"].as_array().unwrap() {
            let mut decoder = MessageDecoder::default();
            for seed in session["seeds"].as_array().unwrap() {
                let _ = decoder.remember_call(seed.as_str().unwrap());
            }
            for raw in session["messages"].as_array().unwrap() {
                let bits: [bool; 77] = raw["bits"]
                    .as_str()
                    .unwrap()
                    .bytes()
                    .map(|b| b == b'1')
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap();
                if let Ok(message) = decoder.unpack(&bits) {
                    let signal = DecodedSignal {
                        message,
                        source_bits: bits,
                        frequency_hz: 1500.,
                        snr_db: -10,
                        dt_seconds: 0.,
                        assisted: false,
                        diagnostics: None,
                    };
                    let mut value = signal_json(&signal, Mode::Ft8, 0, None);
                    value["input"] = session["name"].clone();
                    value["bits"] = raw["bits"].clone();
                    println!("{value}");
                }
            }
        }
        return;
    }
    for text in args {
        match message_encode::encode(&text) {
            Ok(bits) => match decoder.unpack(&bits) {
                Ok(message) => {
                    let signal = DecodedSignal {
                        message,
                        source_bits: bits,
                        frequency_hz: 1500.,
                        snr_db: -10,
                        dt_seconds: 0.,
                        assisted: false,
                        diagnostics: None,
                    };
                    let mut value = signal_json(&signal, Mode::Ft8, 0, None);
                    value["input"] = serde_json::json!(text);
                    value["bits"] = serde_json::json!(
                        bits.iter()
                            .map(|b| if *b { '1' } else { '0' })
                            .collect::<String>()
                    );
                    println!("{value}");
                }
                Err(e) => println!(
                    "{}",
                    serde_json::json!({"input":text,"error":format!("{e:?}")})
                ),
            },
            Err(e) => println!(
                "{}",
                serde_json::json!({"input":text,"error":format!("{e:?}")})
            ),
        }
    }
}
