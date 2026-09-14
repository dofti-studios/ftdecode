// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure Rust FT8/FT4 audio decoding, message parsing, and framed streaming.
//!
//! Experimental Rust APIs; compatibility is not yet guaranteed.

#![doc = include_str!("../docs/library.md")]

pub mod assistance;
pub mod audio_input;
pub mod decode;
pub mod downsample;
pub mod engine;
pub mod fec;
pub mod fft;
pub mod message;
pub mod message_encode;
pub mod osd;
pub mod output;
pub mod protocol;
pub mod stream;
pub mod symbols;
mod tables;
pub mod wav;
pub mod wav_cli;
