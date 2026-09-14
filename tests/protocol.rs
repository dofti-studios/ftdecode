use ftdecode::protocol::{read_frame, write_frame};
use serde_json::json;
use std::io::{self, Cursor, Read};

struct Fragmented(Cursor<Vec<u8>>);
impl Read for Fragmented {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = bytes.len().min(1);
        self.0.read(&mut bytes[..n])
    }
}

#[test]
fn fragmented_and_coalesced_frames() {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &json!({"type":"finish"})).unwrap();
    write_frame(
        &mut bytes,
        &json!({"type":"reset","slot_offset_samples":"0"}),
    )
    .unwrap();
    let mut reader = Fragmented(Cursor::new(bytes));
    assert_eq!(
        read_frame(&mut reader).unwrap().unwrap().header["type"],
        "finish"
    );
    assert_eq!(
        read_frame(&mut reader).unwrap().unwrap().header["type"],
        "reset"
    );
    assert!(read_frame(&mut reader).unwrap().is_none());
}

#[test]
fn reject_lengths_before_reading_payload() {
    for length in [0u32, 8193, u32::MAX] {
        assert!(read_frame(&mut Cursor::new(length.to_le_bytes())).is_err());
    }
    for header in [
        json!({"type":"audio","payload_bytes":24002}),
        json!({"type":"audio","payload_bytes":3}),
        json!({"type":"finish","payload_bytes":2}),
        json!({"type":"audio","payload_bytes":-1}),
        json!({"type":"audio","payload_bytes":1.5}),
    ] {
        let header = serde_json::to_vec(&header).unwrap();
        let mut frame = (header.len() as u32).to_le_bytes().to_vec();
        frame.extend(header);
        assert_eq!(
            read_frame(&mut Cursor::new(frame)).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn reject_truncated_header_and_payload() {
    assert_eq!(
        read_frame(&mut Cursor::new(vec![1, 0])).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    let header = br#"{"type":"audio","payload_bytes":2}"#;
    let mut frame = (header.len() as u32).to_le_bytes().to_vec();
    frame.extend(header);
    frame.push(1);
    assert_eq!(
        read_frame(&mut Cursor::new(frame)).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
}
