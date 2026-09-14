# Rust library usage

The Rust API is experimental. Use `engine::Decoder` for complete audio decoding;
message packing and FEC modules expose lower-level operations with separate
validation and acceptance responsibilities.

## Decode one window

This example decodes the committed clean FT8 fixture. Run it from the repository
root; applications should provide their own input path.

```rust
use ftdecode::{
    engine::{DecodeSettings, Decoder, Mode},
    wav::WavReader,
};
use std::{fs::File, io::BufReader, sync::atomic::AtomicBool};

let mut wav = WavReader::new(BufReader::new(File::open(
    "tests/fixtures/ft8-clean.wav",
)?))?;
let mut pcm = Vec::new();
loop {
    let chunk = wav.read_samples(12_000)?; // Mono 12 kHz signed 16-bit WAV only.
    if chunk.is_empty() { break; }
    pcm.extend(chunk.into_iter().map(f32::from));
}
let mut decoder = Decoder::new(Mode::Ft8);
let cancel = AtomicBool::new(false);
let mut messages = Vec::new();
let stats = decoder.decode(
    &pcm,
    &DecodeSettings::default(),
    &cancel,
    &mut |signal| messages.push(signal.message.text),
)?;
assert!(!stats.cancelled);
assert!(messages.iter().any(|message| message == "CQ K1ABC FN42"));
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Audio and timing contract

`Decoder::decode` is synchronous and accepts mono 12 kHz `f32` samples in raw
PCM amplitude units. Every sample must be finite and within `[-32768, 32768]`.
Convert normalized `[-1, 1]` audio by multiplying by 32768; converting `i16` with
`f32::from` already gives the correct scale.

| Mode | `period_samples()` | `input_samples()` |
| --- | --- | --- |
| FT8 | 180,000 (15 s) | 180,000 (15 s) |
| FT4 | 90,000 (7.5 s) | 72,576 (6.048 s) |

Pass exactly `input_samples()` samples beginning at a slot boundary, then advance
by `period_samples()` for the next window. Wrong lengths return an error. This
API does not find UTC boundaries, resample audio or fill incomplete windows.

For general WAV conversion, `audio_input::ConvertedWav` selects a channel and
converts to 12 kHz PCM-scale floats in bounded chunks. Its filter can overshoot
the public decoder's amplitude bounds. The CLI's `wav_cli::WavFrames` and
`stream::serve_wav` route uses a separate, larger validated bound for these
filtered transients; use that route to reproduce CLI conversion behavior.
`WavFrames` contains internal float frames and must not be sent to TCP or stdio.

Reuse one decoder per stream to retain callsign resolution and FT8 list history.
`reset(mode)` discards history and cached decoders. By default each decode call
advances a consecutive period counter. Call `set_slot(period_id)` before each
attempt when slots are skipped or repeated. Identities count actual periods,
including undecoded periods; use the same identity for early/final attempts.
FT8 list history clears on backward jumps or forward jumps larger than two
periods. Callers supplying early audio must still provide a complete input array;
the framed stream layer handles its own early-window padding.

## Settings and assistance

Defaults: 200–3000 Hz, depth 3, one worker, AP off. Search bounds must be finite,
within 0–5000 Hz and at least 50 Hz wide. Depth is 1–3. FT8 accepts 1–12 workers;
FT4 accepts exactly one. Invalid settings return errors.

Direct Rust callers must set **both** `settings.assistance = true` and
`settings.ap.mode = ApMode::Auto` (or `ApMode::Cq`) to enable AP. Changing only one
leaves it off. CLI and protocol adapters synchronize these settings for you.

FT4 AP requires depth 2 or 3. Learned a7/a8 history decoding is FT8-only.
For contact AP, supply `ap.my_call`, `ap.dx_call`, the appropriate `ap.state`, and
`priority_hz` as the RX audio target. `ap.width_hz` defaults to 50 Hz.
FT8 can additionally target `ap.tx_hz`; FT4 contact AP uses RX only.
`ap.dx_grid` supplies context for FT8 a8 list decoding. Known calls seed hash
resolution even when AP is off; they do not independently add AP trials.
Activity and contact state gate hypotheses, so enabling AP does not guarantee
an assisted result. The protocol guide contains the full settings table.

## Results and cancellation

Callbacks run on the calling thread. With one worker they arrive during the
search. With multiple FT8 workers, results are buffered until all workers finish
and then merged in frequency-band order; the first callback is delayed.

Set the shared `AtomicBool` to true to request cooperative cancellation. A
callback can set it too, but in parallel mode the searches have already finished
and this stops subsequent delivery. Cancellation is not instantaneous. Inspect
`DecodeStats.cancelled`; a successful `Result` can describe a cancelled search.
Use a fresh or cleared flag before the next decode. Partial callback results
already delivered are not retracted.

`DecodedSignal` supplies source bits after FT4 descrambling, parsed message,
audio frequency in Hz, estimated SNR in dB, and timing offset in seconds from
nominal mode transmit start. SNR is not calibrated RF power. The message's
`report_db` is a transmitted report, distinct from measured receive SNR.
Diagnostics describe FEC attempts, not measured channel BER. Optional confidence
is an internal assistance metric, not a calibrated probability.

## Message encoder

`message_encode::encode` synthesizes supported payloads for receive assistance.
It does not transmit. Standard signed reports accept -50 through +50 dB.
DXpedition reports accept -30 through +32 dB in 2 dB steps: odd values round down
(`-11` → `-12`, `+11` → `+10`). This native quantization is intentional.
RTTY reports are 529, 539, …, 599; VHF reports are 52–59 with serials 1–2047.
Other field limits and compound-call normalization are documented on `encode`.
Free text, telemetry and WSPR synthesis are unsupported by this API.
