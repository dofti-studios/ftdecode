# Attribution

ftdecode is a Rust derivative of [WSJT-X](https://github.com/WSJTX/wsjtx),
licensed under GPL-3.0-or-later. The port began on 2026-09-10 and is based on
revision `ccdfaf3c1c109010d15399674ce278167cfde848`. See [LICENSE](LICENSE).

The port replaces native shared state with Rust-owned buffers and adds input
validation, WAV conversion, streaming, cancellation and parallel orchestration.
The source files below are modified translations, not unmodified upstream code.

| Rust source | WSJT-X source |
| --- | --- |
| `src/fec.rs` | `lib/ft8/{encode174_91,get_crc14,bpdecode174_91,decode174_91}.f90`, `lib/platanh.f90` |
| `src/tables.rs` | `lib/ft8/ldpc_174_91_c_*.f90` |
| `src/symbols.rs` | `lib/ft8/genft8.f90`, `lib/ft4/genft4.f90` |
| `src/osd.rs` | `lib/ft8/osd174_91.f90`, `lib/indexx.f90` |
| `src/message.rs`, `src/message_tables.rs`, `src/message_encode.rs` | `lib/77bit/packjt77.f90` |
| `src/assistance.rs` | `lib/ft8/{ft8b,ft8apset}.f90`, `lib/ft4_decode.f90`, `stdcall` in `lib/qra/q65/q65_set_list.f90` |
| `src/decode/ft8_assistance.rs` | `lib/ft8/{ft8_a7,ft8_a8d}.f90`, `stdcall` in `lib/qra/q65/q65_set_list.f90` |
| `src/downsample.rs` | `lib/ft8/ft8_downsample.f90`, `lib/ft4/{ft4_downsample,ft4_params}.f90` |
| `src/decode/ft8.rs` | `lib/ft8_decode.f90`, `lib/ft8/{sync8,sync8d,ft8b,baseline,get_spectrum_baseline,subtractft8,gen_ft8wave}.f90` and their window, polynomial-baseline, frequency-tweak and Gaussian-pulse helpers |
| `src/decode/ft4.rs` | `lib/ft4_decode.f90`, `lib/ft4/{getcandidates4,ft4_baseline,sync4d,get_ft4_bitmetrics,subtractft4,gen_ft4wave}.f90`, `lib/nuttal_window.f90`, `lib/ft8/twkfreq1.f90`, `normalizebmet` in `lib/ft8/ft8b.f90`, `lib/ft2/gfsk_pulse.f90` |

Upstream's `doc/common/license.adoc` supplies the GPL-3.0-or-later license
and this copyright notice:

> The algorithms, source code, look-and-feel of WSJT-X and related
> programs, and protocol specifications for the modes FSK441, FST4,
> FST4W, FT4, FT8, JT4, JT6M, JT9, JT44, JT65, JTMS, Q65, QRA64, ISCAT,
> and MSK144 are Copyright (C) 2001-2026 by one or more of the following
> authors: Joseph Taylor, K1JT; Bill Somerville, G4WJS; Steven Franke,
> K9AN; Nico Palermo, IV3NWV; Uwe Risse, DG2YCB; Brian Moran, N9ADG;
> John Nelson, G4KLA; Charles Suckling, DL3WDG; Roger Rehr, W3SZ;
> Greg Beam, KI7MT; Michael Black, W9MDB; Edson Pereira, PY2SDR; Philip
> Karn, KA9Q; and other members of the WSJT Development Group.

Fixtures in `tests/fixtures` contain synthetic audio and reference vectors, with
upstream revisions and available generation metadata in the accompanying JSON.
Regeneration records are incomplete. No off-air recordings or callsign database
are included. Rust dependency versions are recorded in `Cargo.lock`; dependencies
retain their own licenses.
