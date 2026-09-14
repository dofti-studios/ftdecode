use ftdecode::engine::{DecodeSettings, DecodeStats, DecodedSignal, Mode};
use ftdecode::protocol::read_frame;
use ftdecode::stream::{Backend, serve_with_decoder};
use serde_json::{Value, json};
use std::io::{self, Cursor, Read, Write};
use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};
use std::time::Duration;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
type Calls = Arc<Mutex<Vec<(Mode, usize, u8, f32)>>>;
struct Fake {
    calls: Calls,
    mode: Mode,
}
impl Backend for Fake {
    fn reset(&mut self, mode: Mode) {
        self.mode = mode;
    }
    fn decode(
        &mut self,
        pcm: &[f32],
        settings: &DecodeSettings,
        _: &AtomicBool,
        _: &mut dyn FnMut(DecodedSignal),
    ) -> Result<DecodeStats, String> {
        self.calls
            .lock()
            .unwrap()
            .push((self.mode, pcm.len(), settings.depth, pcm[0]));
        Ok(DecodeStats::default())
    }
}
fn frame(bytes: &mut Vec<u8>, mut v: Value, payload: &[u8]) {
    v["payload_bytes"] = json!(payload.len());
    let h = serde_json::to_vec(&v).unwrap();
    bytes.extend((h.len() as u32).to_le_bytes());
    bytes.extend(h);
    bytes.extend(payload);
}
fn start(bytes: &mut Vec<u8>, mode: &str) {
    start_operation(bytes, mode, "offline");
}
fn start_operation(bytes: &mut Vec<u8>, mode: &str, operation: &str) {
    frame(
        bytes,
        json!({"type":"start","version":1,"mode":mode,"operation":operation,"slot_offset_samples":"0"}),
        &[],
    );
}
fn audio(bytes: &mut Vec<u8>, mut first: u64, mut count: usize) {
    while count > 0 {
        let n = count.min(12000);
        let payload: Vec<_> = (0..n).flat_map(|_| 123i16.to_le_bytes()).collect();
        frame(
            bytes,
            json!({"type":"audio","first_sample":first.to_string(),"sample_count":n}),
            &payload,
        );
        first += n as u64;
        count -= n;
    }
}
fn run(bytes: Vec<u8>) -> (Vec<Value>, Calls) {
    let output = Capture::default();
    let calls = Calls::default();
    serve_with_decoder(
        Cursor::new(bytes),
        output.clone(),
        Fake {
            calls: calls.clone(),
            mode: Mode::Ft8,
        },
    )
    .unwrap();
    let mut input = Cursor::new(output.0.lock().unwrap().clone());
    let mut events = Vec::new();
    while let Some(f) = read_frame(&mut input).unwrap() {
        events.push(f.header);
    }
    (events, calls)
}
fn finish(bytes: &mut Vec<u8>) {
    frame(bytes, json!({"type":"finish"}), &[]);
}

#[test]
fn stream_ap_is_off_unless_explicitly_enabled() {
    use ftdecode::assistance::ApMode;
    struct ApCapture(Arc<Mutex<Vec<ApMode>>>);
    impl Backend for ApCapture {
        fn reset(&mut self, _: Mode) {}
        fn decode(
            &mut self,
            _: &[f32],
            settings: &DecodeSettings,
            _: &AtomicBool,
            _: &mut dyn FnMut(DecodedSignal),
        ) -> Result<DecodeStats, String> {
            self.0.lock().unwrap().push(settings.ap_mode());
            Ok(DecodeStats::default())
        }
    }
    for (settings, expected) in [
        (json!({}), ApMode::Off),
        (
            json!({"my_call":"W9XYZ","dx_call":"K1ABC","rx_hz":1500}),
            ApMode::Off,
        ),
        (json!({"ap":"auto"}), ApMode::Auto),
        (json!({"ap":"cq"}), ApMode::Cq),
        (json!({"assistance":true}), ApMode::Auto),
        (json!({"assistance":false}), ApMode::Off),
    ] {
        let mut input = Vec::new();
        frame(
            &mut input,
            json!({"type":"start","version":1,"mode":"ft4",
            "operation":"offline","slot_offset_samples":"0","settings":settings}),
            &[],
        );
        audio(&mut input, 0, 90000);
        finish(&mut input);
        let seen = Arc::new(Mutex::new(Vec::new()));
        serve_with_decoder(
            Cursor::new(input),
            Capture::default(),
            ApCapture(seen.clone()),
        )
        .unwrap();
        assert_eq!(*seen.lock().unwrap(), [expected], "{settings}");
    }
}

#[test]
fn offline_ft8_decodes_each_full_slot_once_and_skips_incomplete_tail() {
    for depth in [1, 2, 3] {
        let mut input = Vec::new();
        start(&mut input, "ft8");
        frame(
            &mut input,
            json!({"type":"configure","settings":{"depth":depth}}),
            &[],
        );
        audio(&mut input, 0, 360000 + 162432);
        finish(&mut input);
        let (events, calls) = run(input);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(Mode::Ft8, 180000, depth, 123.0); 2],
            "offline depth {depth} must decode only complete slots"
        );
        let terminals: Vec<_> = events.iter().filter(|e| e["type"] == "done").collect();
        assert_eq!(terminals.len(), 3);
        for (terminal, boundary) in terminals[..2].iter().zip(["0", "180000"]) {
            assert_eq!(terminal["status"], "completed");
            assert_eq!(terminal["window_start_sample"], boundary);
        }
        assert_eq!(terminals[2]["window_start_sample"], "360000");
        assert_eq!(terminals[2]["status"], "skipped");
        assert_eq!(terminals[2]["reason"], "incomplete tail");
        assert_eq!(terminals[2]["message_count"], 0);
    }
}

#[test]
fn ft4_decodes_early_and_retains_period_boundaries() {
    let mut input = Vec::new();
    start(&mut input, "ft4");
    audio(&mut input, 0, 180000);
    finish(&mut input);
    let (events, calls) = run(input);
    assert_eq!(
        *calls.lock().unwrap(),
        vec![(Mode::Ft4, 72576, 3, 123.0); 2]
    );
    let starts: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "done")
        .map(|e| e["window_start_sample"].clone())
        .collect();
    assert_eq!(starts, vec![json!("0"), json!("90000")]);
    assert_eq!(events.last().unwrap()["command"], "finish");
}

#[test]
fn configure_preserves_open_window_and_finish_reports_tail() {
    let mut input = Vec::new();
    start(&mut input, "ft8");
    audio(&mut input, 0, 600);
    frame(
        &mut input,
        json!({"type":"configure","settings":{"depth":1}}),
        &[],
    );
    audio(&mut input, 600, 360000);
    finish(&mut input);
    let (events, calls) = run(input);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].2, 3);
    assert_eq!(calls[1].2, 1);
    assert!(
        events
            .iter()
            .any(|e| e["reason"] == "incomplete tail" && e["window_start_sample"] == "360000")
    );
}

#[test]
fn gap_discards_window_and_overlap_is_rejected() {
    let mut input = Vec::new();
    start(&mut input, "ft4");
    audio(&mut input, 0, 12000);
    audio(&mut input, 13000, 12000);
    audio(&mut input, 24000, 10);
    audio(&mut input, 90000, 72576);
    finish(&mut input);
    let (events, calls) = run(input);
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(
        events
            .iter()
            .any(|e| e["reason"] == "input_gap" && e["first_sample"] == "12000")
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "error" && e["command"] == "audio")
    );
    assert!(
        events
            .iter()
            .any(|e| e["status"] == "completed" && e["window_start_sample"] == "90000")
    );
}

#[test]
fn utc_boundary_uses_exact_integer_ceiling() {
    let mut input = Vec::new();
    frame(
        &mut input,
        json!({"type":"start","version":1,"mode":"ft4","operation":"offline","start_utc_ns":"7499999999"}),
        &[],
    );
    audio(&mut input, 0, 72577);
    finish(&mut input);
    let (events, calls) = run(input);
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(
        events
            .iter()
            .any(|e| e["status"] == "completed" && e["window_start_sample"] == "1")
    );
}

#[test]
fn settings_and_timing_are_strict() {
    let mut input = Vec::new();
    start(&mut input, "ft8");
    for settings in [
        json!({"threads":0}),
        json!({"assistance":"yes"}),
        json!({"typo":1}),
        json!({"depth":1.5}),
    ] {
        frame(
            &mut input,
            json!({"type":"configure","settings":settings}),
            &[],
        );
    }
    frame(
        &mut input,
        json!({"type":"reset","start_utc_ns":"1","slot_offset_samples":"0"}),
        &[],
    );
    finish(&mut input);
    let (events, _) = run(input);
    assert_eq!(events.iter().filter(|e| e["type"] == "error").count(), 5);
}

#[test]
fn ft4_thread_settings_are_rejected_before_session_changes() {
    for command in ["start", "reset", "configure"] {
        let mut input = Vec::new();
        let (mode, samples) = if command == "reset" {
            frame(
                &mut input,
                json!({"type":"start","version":1,"mode":"ft8",
                "operation":"offline","slot_offset_samples":"0","settings":{"threads":2}}),
                &[],
            );
            frame(
                &mut input,
                json!({"type":"reset","mode":"ft4","slot_offset_samples":"0"}),
                &[],
            );
            (Mode::Ft8, 180000)
        } else {
            if command == "start" {
                frame(
                    &mut input,
                    json!({"type":"start","version":1,"mode":"ft4",
                    "operation":"offline","slot_offset_samples":"0","settings":{"threads":2}}),
                    &[],
                );
            }
            start(&mut input, "ft4");
            if command == "configure" {
                frame(
                    &mut input,
                    json!({"type":"configure","settings":{"threads":2,"depth":1}}),
                    &[],
                );
            }
            (Mode::Ft4, 72576)
        };
        audio(&mut input, 0, samples);
        finish(&mut input);
        let (events, calls) = run(input);
        let errors: Vec<_> = events.iter().filter(|e| e["type"] == "error").collect();
        assert_eq!(errors.len(), 1, "{command}: {events:?}");
        assert_eq!(errors[0]["command"], command, "{events:?}");
        assert_eq!(errors[0]["message"], "FT4 supports threads=1", "{events:?}");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(mode, samples, 3, 123.0)],
            "{command}"
        );
    }
}

#[test]
fn known_call_settings_are_bounded_normalized_and_updated_atomically() {
    struct CaptureSettings(Arc<Mutex<Vec<DecodeSettings>>>);
    impl Backend for CaptureSettings {
        fn reset(&mut self, _: Mode) {}
        fn decode(
            &mut self,
            _: &[f32],
            settings: &DecodeSettings,
            _: &AtomicBool,
            _: &mut dyn FnMut(DecodedSignal),
        ) -> Result<DecodeStats, String> {
            self.0.lock().unwrap().push(settings.clone());
            Ok(DecodeStats::default())
        }
    }
    let mut input = Vec::new();
    start(&mut input, "ft4");
    for count in [400, 401] {
        frame(
            &mut input,
            json!({"type":"configure","settings":{"known_calls":vec!["  k1abc  "; count]}}),
            &[],
        );
    }
    audio(&mut input, 0, 72576);
    finish(&mut input);
    let output = Capture::default();
    let settings = Arc::new(Mutex::new(Vec::new()));
    serve_with_decoder(
        Cursor::new(input),
        output.clone(),
        CaptureSettings(settings.clone()),
    )
    .unwrap();
    let settings = settings.lock().unwrap();
    assert_eq!(settings.len(), 1);
    assert_eq!(settings[0].ap.known_calls.len(), 400);
    assert!(
        settings[0]
            .ap
            .known_calls
            .iter()
            .all(|call| call == "K1ABC")
    );
    let captured = output.0.lock().unwrap();
    let mut reader = &captured[..];
    let mut errors = Vec::new();
    while let Some(frame) = read_frame(&mut reader).unwrap() {
        if frame.header["type"] == "error" {
            errors.push(frame.header);
        }
    }
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["command"], "configure");
}

#[test]
fn reset_ack_is_an_output_barrier() {
    let mut input = Vec::new();
    start(&mut input, "ft8");
    audio(&mut input, 0, 180000);
    frame(
        &mut input,
        json!({"type":"reset","mode":"ft4","slot_offset_samples":"0"}),
        &[],
    );
    audio(&mut input, 0, 72576);
    finish(&mut input);
    let (events, calls) = run(input);
    let ack = events
        .iter()
        .position(|e| e["type"] == "ok" && e["command"] == "reset")
        .unwrap();
    assert_eq!(events[ack + 1]["type"], "done");
    assert_eq!(events[ack + 1]["status"], "completed");
    assert_eq!(events[ack + 2]["command"], "finish");
    assert_eq!(calls.lock().unwrap().last().unwrap().0, Mode::Ft4);
}

#[test]
fn publishes_before_finish_and_reset_cancels_active_decode() {
    use ftdecode::message::{Message, MessageKind};
    use std::io::Read;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    struct Active;
    impl Backend for Active {
        fn reset(&mut self, _: Mode) {}
        fn decode(
            &mut self,
            _: &[f32],
            _: &DecodeSettings,
            cancel: &AtomicBool,
            publish: &mut dyn FnMut(DecodedSignal),
        ) -> Result<DecodeStats, String> {
            publish(DecodedSignal {
                source_bits: std::array::from_fn(|i| i % 3 == 0),
                message: Message {
                    text: "CQ TEST".into(),
                    kind: MessageKind::FreeText,
                    grid: None,
                    hashes: vec![],
                    fields: Default::default(),
                },
                frequency_hz: 1000.0,
                snr_db: -10,
                dt_seconds: 0.0,
                assisted: false,
                diagnostics: None,
            });
            let deadline = Instant::now() + Duration::from_secs(3);
            while !cancel.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "reset did not cancel active decode"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(DecodeStats {
                cancelled: true,
                ..Default::default()
            })
        }
    }
    struct PipeReader {
        rx: mpsc::Receiver<Vec<u8>>,
        buffer: Cursor<Vec<u8>>,
    }
    impl Read for PipeReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.buffer.position() as usize == self.buffer.get_ref().len() {
                self.buffer =
                    Cursor::new(self.rx.recv_timeout(Duration::from_secs(4)).map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "test pipe timed out")
                    })?);
            }
            self.buffer.read(out)
        }
    }
    struct PipeWriter(mpsc::Sender<Vec<u8>>);
    impl Write for PipeWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.send(bytes.to_vec()).unwrap();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (input, input_rx) = mpsc::channel();
    let (output, output_rx) = mpsc::channel();
    let mut client = PipeReader {
        rx: output_rx,
        buffer: Cursor::new(Vec::new()),
    };
    let reader = PipeReader {
        rx: input_rx,
        buffer: Cursor::new(Vec::new()),
    };
    let worker = std::thread::spawn(move || serve_with_decoder(reader, PipeWriter(output), Active));
    let mut bytes = Vec::new();
    start(&mut bytes, "ft4");
    input.send(bytes.clone()).unwrap();
    assert_eq!(
        read_frame(&mut client).unwrap().unwrap().header["command"],
        "start"
    );
    bytes.clear();
    audio(&mut bytes, 0, 72576);
    input.send(bytes.clone()).unwrap();
    assert_eq!(
        read_frame(&mut client).unwrap().unwrap().header["type"],
        "decode"
    );
    bytes.clear();
    frame(
        &mut bytes,
        json!({"type":"reset","slot_offset_samples":"0"}),
        &[],
    );
    input.send(bytes.clone()).unwrap();
    assert_eq!(
        read_frame(&mut client).unwrap().unwrap().header["status"],
        "cancelled"
    );
    assert_eq!(
        read_frame(&mut client).unwrap().unwrap().header["command"],
        "reset"
    );
    bytes.clear();
    finish(&mut bytes);
    input.send(bytes).unwrap();
    assert_eq!(
        read_frame(&mut client).unwrap().unwrap().header["command"],
        "finish"
    );
    worker.join().unwrap().unwrap();
}

#[test]
fn live_overload_keeps_latest_pending_window_and_reports_every_skip() {
    use std::io::Read;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use std::time::{Duration, Instant};
    struct Tracking {
        input: Cursor<Vec<u8>>,
        read: Arc<AtomicUsize>,
    }
    impl Read for Tracking {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.input.read(out)?;
            self.read.fetch_add(n, Ordering::Release);
            Ok(n)
        }
    }
    struct BlockOnce(Option<mpsc::Receiver<()>>);
    impl Backend for BlockOnce {
        fn reset(&mut self, _: Mode) {}
        fn decode(
            &mut self,
            _: &[f32],
            _: &DecodeSettings,
            _: &AtomicBool,
            _: &mut dyn FnMut(DecodedSignal),
        ) -> Result<DecodeStats, String> {
            if let Some(release) = self.0.take() {
                release.recv_timeout(Duration::from_secs(4)).unwrap();
            }
            Ok(DecodeStats::default())
        }
    }
    let mut input = Vec::new();
    frame(
        &mut input,
        json!({"type":"start","version":1,"mode":"ft4","operation":"live","slot_offset_samples":"0"}),
        &[],
    );
    audio(&mut input, 0, 900000);
    finish(&mut input);
    let total = input.len();
    let read = Arc::new(AtomicUsize::new(0));
    let reader = Tracking {
        input: Cursor::new(input),
        read: read.clone(),
    };
    let output = Capture::default();
    let capture = output.clone();
    let (release, wait) = mpsc::channel();
    let worker =
        std::thread::spawn(move || serve_with_decoder(reader, output, BlockOnce(Some(wait))));
    let deadline = Instant::now() + Duration::from_secs(3);
    while read.load(Ordering::Acquire) < total {
        assert!(
            Instant::now() < deadline,
            "live ingress blocked behind decoder"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    let mut input = Cursor::new(capture.0.lock().unwrap().clone());
    let mut completed = Vec::new();
    let mut skipped = Vec::new();
    while let Some(f) = read_frame(&mut input).unwrap() {
        if f.header["type"] == "done" {
            let start = f.header["window_start_sample"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            if f.header["status"] == "completed" {
                completed.push(start)
            } else {
                skipped.push(start)
            }
        }
    }
    assert_eq!(completed.last(), Some(&810000));
    assert!(!skipped.is_empty());
    completed.extend(skipped);
    completed.sort_unstable();
    assert_eq!(completed, (0..10).map(|n| n * 90000).collect::<Vec<_>>());
}

#[test]
fn live_stalled_writer_returns_with_bounded_timeout() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    struct Stalled(mpsc::Receiver<()>);
    impl Write for Stalled {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let _ = self.0.recv();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut input = Vec::new();
    frame(
        &mut input,
        json!({"type":"start","version":1,"mode":"ft8","operation":"live","slot_offset_samples":"0"}),
        &[],
    );
    finish(&mut input);
    let (release, wait) = mpsc::channel();
    let started = Instant::now();
    let result = serve_with_decoder(
        Cursor::new(input),
        Stalled(wait),
        Fake {
            calls: Calls::default(),
            mode: Mode::Ft8,
        },
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(7));
    drop(release);
}

struct Gate {
    input: Cursor<Vec<u8>>,
    boundary: usize,
    ready: Option<mpsc::Receiver<()>>,
}
impl Read for Gate {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.input.position() as usize >= self.boundary
            && let Some(ready) = self.ready.take()
        {
            ready
                .recv_timeout(Duration::from_secs(3))
                .expect("FT8 did not publish at 13.536 seconds");
        }
        self.input.read(bytes)
    }
}

fn progressive(tail: &str) -> Vec<Value> {
    use ftdecode::message::{Message, MessageKind};
    use std::sync::mpsc;
    struct Progressive(Option<mpsc::Sender<()>>);
    impl Backend for Progressive {
        fn reset(&mut self, _: Mode) {}
        fn decode(
            &mut self,
            pcm: &[f32],
            settings: &DecodeSettings,
            _: &AtomicBool,
            publish: &mut dyn FnMut(DecodedSignal),
        ) -> Result<DecodeStats, String> {
            assert_eq!(pcm.len(), 180000);
            assert_eq!(
                settings.depth, 3,
                "configuration changed an already opened window"
            );
            let signal = DecodedSignal {
                // Stable recovered bits; learned history changes only display.
                source_bits: std::array::from_fn(|i| i < 22 && ((123_u32 >> i) & 1) != 0),
                message: Message {
                    text: if pcm[170000] == 0.0 {
                        "CQ <...>"
                    } else {
                        "CQ <K1ABC>"
                    }
                    .into(),
                    kind: MessageKind::Standard,
                    grid: None,
                    fields: Default::default(),
                    hashes: vec![ftdecode::message::CallsignHash {
                        width: 22,
                        value: 123,
                        resolved: if pcm[170000] == 0.0 {
                            None
                        } else {
                            Some("K1ABC".into())
                        },
                    }],
                },
                frequency_hz: 1000.0,
                snr_db: -10,
                dt_seconds: 0.0,
                assisted: false,
                diagnostics: None,
            };
            publish(signal.clone());
            if pcm[170000] != 0.0 {
                publish(DecodedSignal {
                    frequency_hz: 1500.0,
                    ..signal
                });
            }
            if let Some(ready) = self.0.take() {
                ready.send(()).unwrap();
            }
            Ok(DecodeStats::default())
        }
    }
    let mut input = Vec::new();
    start_operation(&mut input, "ft8", "live");
    audio(&mut input, 0, 162432);
    let boundary = input.len();
    match tail {
        "final" => {
            frame(
                &mut input,
                json!({"type":"configure","settings":{"depth":1}}),
                &[],
            );
            audio(&mut input, 162432, 17568);
        }
        "gap" => audio(&mut input, 163432, 1000),
        "reset" => frame(
            &mut input,
            json!({"type":"reset","slot_offset_samples":"0"}),
            &[],
        ),
        "finish" => {}
        _ => unreachable!(),
    }
    finish(&mut input);
    let (ready, wait) = mpsc::channel();
    let capture = Capture::default();
    serve_with_decoder(
        Gate {
            input: Cursor::new(input),
            boundary,
            ready: Some(wait),
        },
        capture.clone(),
        Progressive(Some(ready)),
    )
    .unwrap();
    let mut input = Cursor::new(capture.0.lock().unwrap().clone());
    let mut events = Vec::new();
    while let Some(f) = read_frame(&mut input).unwrap() {
        events.push(f.header);
    }
    events
}

#[test]
fn ft8_progressive_pass_publishes_early_and_final_deduplicates() {
    let events = progressive("final");
    let decodes: Vec<_> = events.iter().filter(|e| e["type"] == "decode").collect();
    assert_eq!(decodes.len(), 2);
    assert_eq!(decodes[0]["frequency_hz"], 1000.0);
    assert_eq!(decodes[1]["frequency_hz"], 1500.0);
    let terminals: Vec<_> = events.iter().filter(|e| e["type"] == "done").collect();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0]["message_count"], 2);
    assert_eq!(terminals[0]["status"], "completed");
}

#[test]
fn progressive_hash_resolution_does_not_republish_the_same_signal() {
    let events = progressive("final");
    let same_frequency: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "decode" && e["frequency_hz"] == 1000.0)
        .collect();
    assert_eq!(same_frequency.len(), 1);
    assert_eq!(same_frequency[0]["message"], "CQ <...>");
    assert!(same_frequency[0]["hashes"][0]["resolved"].is_null());
    let separate_frequency = events
        .iter()
        .find(|e| e["type"] == "decode" && e["frequency_hz"] == 1500.0)
        .unwrap();
    assert_eq!(separate_frequency["message"], "CQ <K1ABC>");
    assert_eq!(separate_frequency["hashes"][0]["resolved"], "K1ABC");
}

#[test]
fn fatal_framing_delivers_error_and_returns_failure() {
    for (bad_frame, kind) in [
        (vec![0, 0, 0, 0], io::ErrorKind::InvalidData),
        (vec![1, 0], io::ErrorKind::UnexpectedEof),
        (vec![1, 0, 0, 0, b'{'], io::ErrorKind::InvalidData),
    ] {
        let mut input = Vec::new();
        start(&mut input, "ft8");
        input.extend(bad_frame);
        let output = Capture::default();
        let result = serve_with_decoder(
            Cursor::new(input),
            output.clone(),
            Fake {
                calls: Calls::default(),
                mode: Mode::Ft8,
            },
        );
        assert_eq!(result.unwrap_err().kind(), kind);
        let mut frames = Cursor::new(output.0.lock().unwrap().clone());
        assert_eq!(
            read_frame(&mut frames).unwrap().unwrap().header["command"],
            "start"
        );
        let error = read_frame(&mut frames).unwrap().unwrap().header;
        assert_eq!(error["type"], "error");
        assert_eq!(error["command"], "frame");
        assert!(!error["message"].as_str().unwrap().is_empty());
        assert!(read_frame(&mut frames).unwrap().is_none());
    }
}

#[test]
fn progressive_partial_window_terminates_on_gap_reset_or_finish() {
    for tail in ["gap", "reset", "finish"] {
        let events = progressive(tail);
        let terminals: Vec<_> = events.iter().filter(|e| e["type"] == "done").collect();
        assert_eq!(terminals.len(), 1, "{tail}: {events:?}");
        assert_eq!(terminals[0]["message_count"], 1);
        assert_eq!(
            terminals[0]["status"],
            if tail == "reset" {
                "cancelled"
            } else {
                "skipped"
            }
        );
    }
}

#[test]
fn converted_wav_preserves_float_pcm_and_reports_operation_specific_attempts() {
    use ftdecode::stream::{WavOptions, serve_wav_with_decoder};
    struct FloatBackend(Option<mpsc::Sender<()>>);
    impl Backend for FloatBackend {
        fn reset(&mut self, _: Mode) {}
        fn decode(
            &mut self,
            pcm: &[f32],
            _: &DecodeSettings,
            _: &AtomicBool,
            _: &mut dyn FnMut(DecodedSignal),
        ) -> Result<DecodeStats, String> {
            assert_eq!(pcm[0], 0.125);
            assert_eq!(pcm[1], 40000.0, "filter overshoot must not be clipped");
            if let Some(ready) = self.0.take() {
                ready.send(()).unwrap();
            }
            Ok(DecodeStats {
                candidates: 3,
                passes: 2,
                ..Default::default()
            })
        }
    }
    for (operation, count, candidates, passes, status) in [
        ("offline", 180000, 3, 2, "completed"),
        ("offline", 162432, 0, 0, "skipped"),
        ("live", 180000, 6, 4, "completed"),
        ("live", 162432, 3, 2, "skipped"),
    ] {
        let mut bytes = Vec::new();
        start_operation(&mut bytes, "ft8", operation);
        let mut boundary = 0;
        let mut first = 0;
        while first < count {
            let end = if first < 162432 {
                count.min(162432)
            } else {
                count
            };
            let n = (end - first).min(6000);
            let mut values = vec![0.125f32; n];
            if first == 0 {
                values[1] = 40000.0;
            }
            let payload: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
            frame(
                &mut bytes,
                json!({"type":"audio","first_sample":first.to_string(),"sample_count":n}),
                &payload,
            );
            first += n;
            if first == 162432 {
                boundary = bytes.len();
            }
        }
        frame(&mut bytes, json!({"type":"finish"}), &[]);
        let output = Capture::default();
        let (ready, wait) = mpsc::channel();
        serve_wav_with_decoder(
            Gate {
                input: Cursor::new(bytes),
                boundary,
                ready: (operation == "live").then_some(wait),
            },
            output.clone(),
            FloatBackend((operation == "live").then_some(ready)),
            WavOptions {
                diagnostics: 2,
                stats: true,
            },
        )
        .unwrap();
        let captured = output.0.lock().unwrap();
        let mut reader = &captured[..];
        let mut done = None;
        while let Some(frame) = read_frame(&mut reader).unwrap() {
            if frame.header["type"] == "done" {
                assert!(done.is_none(), "duplicate terminal event");
                done = Some(frame.header);
            }
        }
        let done = done.unwrap();
        assert_eq!(done["status"], status, "{operation}, {count}");
        assert_eq!(
            done["stats"]["candidates"], candidates,
            "{operation}, {count}"
        );
        assert_eq!(done["stats"]["passes"], passes, "{operation}, {count}");
        assert_eq!(done["message_count"], 0);
    }
}

#[test]
fn decoder_errors_survive_final_input_and_live_incomplete_finish() {
    struct FailsEarly;
    impl Backend for FailsEarly {
        fn reset(&mut self, _: Mode) {}
        fn decode(
            &mut self,
            _: &[f32],
            _: &DecodeSettings,
            _: &AtomicBool,
            _: &mut dyn FnMut(DecodedSignal),
        ) -> Result<DecodeStats, String> {
            Err("injected progressive decoder failure".into())
        }
    }
    for (operation, count, status, reason) in [
        (
            "live",
            162432,
            "failed",
            "injected progressive decoder failure",
        ),
        (
            "live",
            180000,
            "failed",
            "injected progressive decoder failure",
        ),
        (
            "offline",
            180000,
            "failed",
            "injected progressive decoder failure",
        ),
        ("offline", 162432, "skipped", "incomplete tail"),
    ] {
        let mut bytes = Vec::new();
        start_operation(&mut bytes, "ft8", operation);
        audio(&mut bytes, 0, count);
        frame(&mut bytes, json!({"type":"finish"}), &[]);
        let output = Capture::default();
        serve_with_decoder(Cursor::new(bytes), output.clone(), FailsEarly).unwrap();
        let bytes = output.0.lock().unwrap();
        let mut reader = &bytes[..];
        let mut done = Vec::new();
        while let Some(frame) = read_frame(&mut reader).unwrap() {
            if frame.header["type"] == "done" {
                done.push(frame.header);
            }
        }
        assert_eq!(done.len(), 1);
        assert_eq!(done[0]["status"], status, "{operation}, {count}: {done:?}");
        assert_eq!(done[0]["reason"], reason);
    }
}

#[test]
fn converted_ingress_rejects_invalid_floats_and_accepts_filter_headroom_boundary() {
    use ftdecode::stream::{WavOptions, serve_wav_with_decoder};
    for value in [f32::NAN, f32::INFINITY, 131073.0, 131072.0] {
        let mut bytes = Vec::new();
        start(&mut bytes, "ft4");
        for first in (0..72576usize).step_by(6000) {
            let count = (72576 - first).min(6000);
            let payload: Vec<_> = (0..count).flat_map(|_| value.to_le_bytes()).collect();
            frame(
                &mut bytes,
                json!({"type":"audio","first_sample":first.to_string(),"sample_count":count}),
                &payload,
            );
        }
        frame(&mut bytes, json!({"type":"finish"}), &[]);
        let output = Capture::default();
        let calls = Calls::default();
        serve_wav_with_decoder(
            Cursor::new(bytes),
            output.clone(),
            Fake {
                calls: calls.clone(),
                mode: Mode::Ft4,
            },
            WavOptions::default(),
        )
        .unwrap();
        let captured = output.0.lock().unwrap();
        let mut reader = &captured[..];
        let mut errors = 0;
        while let Some(frame) = read_frame(&mut reader).unwrap() {
            if frame.header["type"] == "error" {
                errors += 1;
            }
        }
        if value == 131072.0 {
            assert_eq!(errors, 0);
            assert_eq!(calls.lock().unwrap().len(), 1);
            assert_eq!(calls.lock().unwrap()[0].3, value);
        } else {
            assert!(errors > 0);
            assert!(calls.lock().unwrap().is_empty());
        }
    }
}
