// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::protocol::read_frame;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    time::Duration,
};
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn send(writer: &mut impl Write, mut header: Value, payload: &[u8]) {
    header["payload_bytes"] = json!(payload.len());
    let bytes = serde_json::to_vec(&header).unwrap();
    // Deliberately fragment the length/header; audio follows independently.
    for byte in (bytes.len() as u32).to_le_bytes() {
        writer.write_all(&[byte]).unwrap();
    }
    for chunk in bytes.chunks(7) {
        writer.write_all(chunk).unwrap();
    }
    writer.write_all(payload).unwrap();
    writer.flush().unwrap();
}
fn audio(writer: &mut impl Write, pcm: &[u8], first: usize) {
    for (i, chunk) in pcm.chunks(24000).enumerate() {
        send(
            writer,
            json!({"type":"audio","first_sample":(first+i*12000).to_string(),"sample_count":chunk.len()/2}),
            chunk,
        );
    }
}
fn start(writer: &mut impl Write, mode: &str, operation: &str) {
    send(
        writer,
        json!({"type":"start","version":1,"mode":mode,"operation":operation,"slot_offset_samples":"0"}),
        &[],
    );
}
fn read_to_finish(reader: &mut impl Read) -> Vec<Value> {
    let mut values = Vec::new();
    loop {
        let event = read_frame(reader)
            .unwrap()
            .expect("finish acknowledgement")
            .header;
        let finish = event["type"] == "ok" && event["command"] == "finish";
        values.push(event);
        if finish {
            break;
        }
    }
    values
}
fn messages(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|e| e["type"] == "decode")
        .cloned()
        .collect()
}

fn assert_cq_metadata(event: &Value) {
    assert_eq!(event["message_type"], "standard");
    assert_eq!(event["exchange_type"], "cq");
    assert_eq!(event["sender"], "K1ABC");
    assert_eq!(event.get("recipient"), Some(&Value::Null));
    assert_eq!(event["grid"], "FN42");
    assert_eq!(event.get("report_db"), Some(&Value::Null));
    assert_eq!(event.get("sender_hash"), Some(&Value::Null));
    assert_eq!(event.get("recipient_hash"), Some(&Value::Null));
}

#[test]
fn real_wav_tcp_and_stdio_results_match_for_both_modes() {
    for mode in ["ft8", "ft4"] {
        let path = format!(
            "{}/tests/fixtures/{mode}-clean.wav",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut wav = ftdecode::wav::WavReader::new(std::fs::File::open(&path).unwrap()).unwrap();
        let mut pcm = Vec::new();
        loop {
            let chunk = wav.read_samples(12000).unwrap();
            if chunk.is_empty() {
                break;
            }
            for s in chunk {
                pcm.extend(s.to_le_bytes());
            }
        }
        let output = Command::new(env!("CARGO_BIN_EXE_ftdecode"))
            .args(["decode", &path, "--mode", mode, "--output", "jsonl"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let wav_events: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(messages(&wav_events).len(), 1, "{wav_events:?}");
        assert_cq_metadata(&messages(&wav_events)[0]);
        let mut process = Process(
            Command::new(env!("CARGO_BIN_EXE_ftdecode"))
                .arg("stdio")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut input = process.0.stdin.take().unwrap();
        let mut output = process.0.stdout.take().unwrap();
        start(&mut input, mode, "offline");
        assert_eq!(
            read_frame(&mut output).unwrap().unwrap().header["command"],
            "start"
        );
        let bytes = pcm.clone();
        let sender = std::thread::spawn(move || {
            audio(&mut input, &bytes, 0);
            send(&mut input, json!({"type":"finish"}), &[]);
        });
        let stdio = read_to_finish(&mut output);
        sender.join().unwrap();
        assert!(process.0.wait().unwrap().success());
        assert_eq!(messages(&stdio), messages(&wav_events));

        let mut server = Process(
            Command::new(env!("CARGO_BIN_EXE_ftdecode"))
                .args(["serve", "--bind", "127.0.0.1:0", "--once"])
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut server_log = BufReader::new(server.0.stderr.take().unwrap());
        let mut line = String::new();
        server_log.read_line(&mut line).unwrap();
        let ready: Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("server startup: {line:?}: {e}"));
        let mut socket = TcpStream::connect(ready["address"].as_str().unwrap()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        start(&mut socket, mode, "offline");
        assert_eq!(
            read_frame(&mut socket).unwrap().unwrap().header["command"],
            "start"
        );
        let mut writer = socket.try_clone().unwrap();
        let sender = std::thread::spawn(move || {
            audio(&mut writer, &pcm, 0);
            send(&mut writer, json!({"type":"finish"}), &[]);
        });
        let tcp = read_to_finish(&mut socket);
        sender.join().unwrap();
        assert!(server.0.wait().unwrap().success());
        assert_eq!(messages(&tcp), messages(&wav_events));
        assert_eq!(tcp.iter().filter(|e| e["type"] == "done").count(), 1);
    }
}

#[test]
fn real_live_results_include_metadata_before_finish_for_both_modes() {
    for (mode, available) in [("ft8", 162432), ("ft4", 72576)] {
        let bytes = std::fs::read(format!("tests/fixtures/{mode}-clean.wav")).unwrap();
        let pcm = &bytes[44..];
        let mut server = Process(
            Command::new(env!("CARGO_BIN_EXE_ftdecode"))
                .args(["serve", "--bind", "127.0.0.1:0", "--once"])
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut server_log = BufReader::new(server.0.stderr.take().unwrap());
        let mut line = String::new();
        server_log.read_line(&mut line).unwrap();
        let ready: Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("server startup: {line:?}: {e}"));
        let mut socket = TcpStream::connect(ready["address"].as_str().unwrap()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        start(&mut socket, mode, "live");
        read_frame(&mut socket).unwrap();
        audio(&mut socket, &pcm[..available * 2], 0);
        let early = read_frame(&mut socket).unwrap().unwrap().header;
        assert_eq!(early["type"], "decode");
        assert_eq!(early["message"], "CQ K1ABC FN42");
        assert_eq!(early["mode"], mode);
        assert_cq_metadata(&early);
        audio(&mut socket, &pcm[available * 2..], available);
        send(&mut socket, json!({"type":"finish"}), &[]);
        let final_events = read_to_finish(&mut socket);
        assert!(messages(&final_events).is_empty());
        let done = final_events.iter().find(|e| e["type"] == "done").unwrap();
        assert_eq!(done["message_count"], 1);
        assert_eq!(done["status"], "completed");
        assert!(server.0.wait().unwrap().success());
    }
}
