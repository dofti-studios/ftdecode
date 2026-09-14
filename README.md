# ftdecode

A standalone FT8/FT4 decoder written in Rust, based on WSJT-X. Decode WAV files
or stream audio over localhost TCP or stdin/stdout. Available as a CLI and Rust
library; no WSJT-X, Fortran or FFTW installation required.

Early development; expect bugs and API changes.

This is an agent-assisted port; performance and decoding results may differ
from WSJT-X. Receive only; SuperFox is not supported.

## Build

Requires Rust 1.98 or newer and a native linker (Visual Studio C++ build tools
on Windows, Xcode Command Line Tools on macOS).

```sh
git clone https://github.com/dofti-studios/ftdecode.git
cd ftdecode
cargo build --release --locked
```

## Decode

```sh
./target/release/ftdecode decode tests/fixtures/ft8-clean.wav --mode ft8
./target/release/ftdecode decode tests/fixtures/ft4-clean.wav --mode ft4
```

Accepts mono/stereo PCM or float WAVs at 12–192 kHz. Stereo defaults to the left
channel. Use `--output jsonl` for structured results.

For longer recordings, supply `--slot-offset-seconds` for the first slot boundary
or `--start-utc-ns` for the timestamp of sample zero. Single-window files assume
sample zero is a slot boundary. Incomplete windows are skipped.

Run `./target/release/ftdecode --help` for all options.

## Stream

```sh
./target/release/ftdecode serve --bind 127.0.0.1:7373
```

In another terminal:

```sh
python3 examples/stream_client.py --port 7373 --mode ft8 --operation live tests/fixtures/ft8-clean.wav
```

TCP and `ftdecode stdio` use a [framed protocol](docs/protocol.md) carrying mono
12 kHz signed 16-bit PCM. See the [Rust library guide](docs/library.md) for direct
integration.

## Tests

```sh
cargo test --release --locked
```

Test fixtures are included; no WSJT-X installation is needed.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE) and [NOTICE.md](NOTICE.md) for attribution.
