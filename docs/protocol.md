# Framed protocol v1

`ftdecode serve` (localhost TCP) and `ftdecode stdio` use this same full-duplex
protocol. They accept mono signed 16-bit little-endian PCM at 12,000 samples/s.
They do not parse WAV files or newline-delimited JSON input. The Python example
in `examples/stream_client.py` implements framing and concurrent result reads.

The TCP server accepts loopback addresses only and services one client at a time.
It defaults to `127.0.0.1:7373`; `--bind 127.0.0.1:0` chooses an available port
and prints a JSON `listening` event with `address` on stderr. `--once` exits after
one connection. Stdio reserves stdout for frames; keep diagnostics on stderr.

## Framing and types

Each frame contains, in order:

1. Four bytes: unsigned little-endian length of the JSON header, excluding these
   four bytes and the payload. Valid range: 1–8,192 bytes.
2. That many bytes of UTF-8 JSON representing an object.
3. Exactly `payload_bytes` binary bytes.

Every header requires string `type` and integer `payload_bytes`. Only `audio`
may carry a nonempty payload: 1–12,000 complete s16le samples, or 2–24,000 bytes.
Every server frame has `payload_bytes: 0`. Reads may fragment any of these parts;
read exactly the declared lengths. Flush complete frames when writing.

Sample positions and UTC timestamps are **decimal strings** representing u64
values (`"0"`, not `0`), without signs or whitespace. Small counts, lengths,
version and worker/depth settings are JSON integers. Frequencies are JSON numbers.
Unknown command fields and settings are rejected. Optional fields may only use
`null` where explicitly allowed below.

## Commands

The JSON examples are header contents; the four-byte prefix must still be added.

Start a stream, then wait for `ok` with `command: "start"`:

```json
{"type":"start","payload_bytes":0,"version":1,"mode":"ft8","operation":"offline","slot_offset_samples":"0","settings":{"ap":"off"}}
```

`version`, `mode` (`ft8` or `ft4`), `operation` (`live` or `offline`) and exactly
one timing field are required. `settings` is optional and defaults as below.

| Timing field | Meaning |
| --- | --- |
| `slot_offset_samples` | First slot boundary relative to sample zero, in 12 kHz samples |
| `start_utc_ns` | UTC Unix-epoch nanoseconds of sample zero, not connection/send time |

UTC timing selects the first mode-period boundary at or after sample zero,
rounded upward to a sample; leading partial-period audio is discarded. Relative
timing discards samples before the supplied boundary. The sample index still
starts at zero and is not renumbered at that boundary.

Send audio in increasing sample order:

```json
{"type":"audio","payload_bytes":1200,"first_sample":"0","sample_count":600}
```

Append 600 s16le samples. The next contiguous frame starts at `"600"`.
`sample_count` must match the payload length. There is no per-audio acknowledgement.
Overlapping/backward frames are rejected. Forward jumps emit `warning` with
`reason: "input_gap"`; affected partial windows are discarded and alignment
advances to the next intact slot. Missing audio before a future relative boundary
does not invalidate that boundary. Not every wholly missing period gets a `done`.

Change settings without clearing history:

```json
{"type":"configure","payload_bytes":0,"settings":{"ap":"auto","depth":3}}
```

Settings are a partial update. A window snapshots settings when its first audio
is assembled, so configure affects subsequent unopened windows. An `ok` with
`command: "configure"` acknowledges the update, not a re-decode of current audio.

Reset timing and history:

```json
{"type":"reset","payload_bytes":0,"mode":"ft4","slot_offset_samples":"0","settings":{"threads":1}}
```

Reset requires an existing stream and exactly one new timing field. `mode` and
`settings` are optional; omitted settings retain their previous values. Operation
is retained and cannot change on reset; do not send `version` or `operation`.
Reset cancels old work, clears decoder/callsign history, discards partial input,
and restarts input indexing at zero. Previously delivered decodes are not
retracted. Wait for `ok` with `command: "reset"` to distinguish the new timeline;
there is no separate stream ID in events.

Finish gracefully:

```json
{"type":"finish","payload_bytes":0}
```

Finish drains queued work, marks any incomplete tail skipped, and ends with
`ok` / `command: "finish"`. Keep reading until this acknowledgement. EOF or a
socket disconnect requests cancellation instead; closing input is not equivalent
to finish. A command validation error emits `error` and permits later commands;
a malformed frame is fatal. On connection failure, terminal events may not arrive.

## Settings

All settings are optional in start; configure/reset update only supplied keys.

| Key | Default | Accepted values / meaning |
| --- | --- | --- |
| `low_hz`, `high_hz` | 200, 3000 | Finite 0–5000 Hz, band at least 50 Hz wide |
| `rx_hz` or `priority_hz` | null | RX audio frequency 0–5000 Hz or null; choose one alias per command; full search band remains active |
| `priority_tolerance_hz` | 10 | 0–1000 Hz |
| `depth` | 3 | Integer 1–3 |
| `threads` | 1 | FT8 integer 1–12; FT4 exactly 1 |
| `ap` | `"off"` | `"off"`, `"cq"`, `"auto"` |
| `assistance` | false | Compatibility boolean; alone maps true to auto and false to off; if supplied with `ap`, they must agree |
| `my_call`, `dx_call` | null | Station calls or null; 3–11 characters excluding optional outer brackets |
| `dx_grid` | null | Four- or six-character Maidenhead locator or null |
| `qso_state` | `"calling-cq"` | `calling-cq`, `calling-station`, `report`, `roger-report`, `rogers`, `signoff` |
| `activity` | `"normal"` | `normal`, `na-vhf`, `eu-vhf`, `field-day`, `rtty`, `ww-digi`, `fox`, `hound`, `arrl-digi` |
| `ap_width_hz` | 50 | Targeting half-width, 0–1000 Hz |
| `tx_hz` | null | FT8 second contact-AP target, 0–5000 Hz or null; ignored for FT4 contact AP |
| `known_calls` | [] | Array replacing previous seeds, at most 400 entries; 3–13 characters excluding optional outer brackets |

Calls must contain letters and digits, use alphanumeric characters and at most
one internal slash, and cannot start/end with a slash. Calls/grids are normalized
to uppercase. Some syntactically accepted calls cannot provide native AP context.
Bounds apply even to inactive hints; switching AP off does not bypass validation.

### Assistance

`cq` requests CQ hypotheses without station hints; activity gates still apply.
`auto` selects the contact-state AP schedule and enables FT8 a7/a8 list decoding.
FT4 AP requires depth 2 or 3. FT4 has no learned-history list decoder.

For ordinary contact AP supply both station calls, `rx_hz` and the current
`qso_state`. For example, settings matching the weak FT4 contact fixture are:

```json
{"ap":"auto","depth":3,"low_hz":1475,"high_hz":1525,"rx_hz":1500,"my_call":"W9XYZ","dx_call":"K1ABC","qso_state":"roger-report"}
```

Hypothesis types by state are: calling-cq 1/2, calling-station or report 2/3,
roger-report or rogers 3/4/5/6 for FT8 and 3/6 for FT4, signoff 3/1/2.
Type 1 is CQ; type 2 needs local-call context; types 3 and above also need the
other call and target proximity. FT8 accepts RX or TX proximity; FT4 uses RX only.

Fox activity disables these AP hypotheses. FT4 disables them for fox, hound and
arrl-digi. FT8 hound hypotheses are restricted to frequencies at or below 950 Hz,
use special call-context gates, bypass ordinary target proximity and omit type 5.
FT8 a7/a8 lists are disabled for fox/hound. a7 uses learned resolved stations;
a8 needs RX frequency, both calls and the other station's grid. Known-call seeds
help resolve hashes even with AP off but do not independently add AP trials.
These are receive hints; no radio control or contact sequencing is performed.

## Windows and flow control

| Mode | Period | Final audio required | Live early attempt |
| --- | --- | --- | --- |
| FT8 | 180,000 samples / 15 s | 180,000 samples | At 162,432 samples / 13.536 s, depth ≥2 |
| FT4 | 90,000 samples / 7.5 s | 72,576 samples / 6.048 s | None |

FT4 audio between its decode-window end and next boundary is ignored. Incomplete
final windows are skipped, even if FT8 emitted an early result. Early/final
results are deduplicated by payload and nearby frequency/timing within a window.
A repeated text message at another frequency or in another period is meaningful.

Offline mode applies backpressure and preserves queued decode work. Live mode
can omit early attempts and replace obsolete pending windows with newer work,
emitting overload warnings and skipped terminal events for displaced final
windows. Queues are bounded; even live input can block. Live uses sample positions
for scheduling, not the host wall clock; callers must pace live replay themselves.

Read results concurrently with writing audio. A stalled reader can block the
worker and then the sender. TCP writes have a five-second timeout; live output
also waits at most five seconds for a frame to be accepted by its writer. There
is no requirement for the server to produce an event every five seconds. The
Python example clears its short handshake timeout for this reason.

## Server events and output units

| `type` | Fields and meaning |
| --- | --- |
| `ok` | `command`: start, configure, reset or finish |
| `decode` | One recovered signal; fields below |
| `done` | `window_start_sample` string, `status`, `message_count` integer, optional `reason` |
| `warning` | `reason`: input_gap or overload; `first_sample` and exclusive `end_sample` strings |
| `error` | `command` and human-readable `message` |

`done.status` is `completed`, `skipped`, `cancelled` or `failed`. Completed can
have zero messages. Skipped can retain an already published early decode; inspect
both status and count. Human-readable reasons are diagnostics, not machine enums.

Every decode carries `mode`, `message`, `message_type`, `frequency_hz` (audio Hz),
`snr_db` (estimated receive SNR), `dt_seconds` (offset from nominal mode transmit
start), `assisted` (boolean), and `window_start_sample` (12 kHz sample index string).
It also carries `exchange_type`, `sender`, `recipient`, `sender_hash`,
`recipient_hash`, `cq_modifier`, `grid`, `report_db`, and `hashes`. These metadata
fields may be null when absent, unresolved or unclassified; `hashes` is an array.
Each hash object has integer `width`/`value` and nullable string `resolved`.
`report_db` describes the transmitted standard-message report, not receive SNR.

`window_start_utc_ns` is included on decodes only when UTC timing is available.
AP results with an AP type also include `ap_type`, `confidence` and `questionable`;
confidence can be null and is not a calibrated probability. Do not assume
optional fields exist or infer contact actions solely from displayed text.

WAV `--output jsonl` uses the same core decode events without binary framing and
can add input metadata, mode/UTC on terminal events, statistics, diagnostics and
payload bits depending on CLI options. TCP/stdin do not accept those WAV-only
options. For WAV commands, exit success can include skipped windows; check `done`
statuses as well as process exit status.
