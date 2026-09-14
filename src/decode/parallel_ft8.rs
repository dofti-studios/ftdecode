// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded FT8 frequency-band parallelism with reusable DSP and receive history.
//!
//! This partitions the Rust receive path; it is not a port of WSJT-X's alternate
//! multithreaded search algorithm. Each worker also searches a 50 Hz guard on
//! either side so adjacent signals can be decoded and subtracted. Only the
//! worker owning a signal's refined frequency can emit it.
//!
//! For deterministic delivery, parallel workers buffer results until all finish,
//! then the caller thread merges them in band order. This delays the first
//! callback, and callback cancellation stops delivery after the searches finish.
//! External cancellation is shared by the running workers. With one thread,
//! callbacks are immediate and the sequential path is used directly.
use super::ft8::Ft8Decoder;
use crate::{
    engine::{DecodeSettings, DecodeStats, DecodedSignal, MAX_FILTERED_PCM},
    message::MessageDecoder,
};
use std::sync::atomic::{AtomicBool, Ordering};

const GUARD_HZ: f32 = 50.0;
// Three acquisition stages, three passes, at most 1,000 candidates per pass.
// This is a defensive delivery bound, not a new candidate-search limit.
const MAX_WORKER_RESULTS: usize = 9000;

#[derive(Clone, Copy)]
struct Band {
    low: f32,
    high: f32,
    last: bool,
}
impl Band {
    fn owns(self, frequency: f32) -> bool {
        frequency >= self.low && (frequency < self.high || (self.last && frequency == self.high))
    }
}

struct Worker {
    decoder: Ft8Decoder,
    messages: MessageDecoder,
}
impl Worker {
    fn new() -> Self {
        Self {
            decoder: Ft8Decoder::new(),
            messages: MessageDecoder::default(),
        }
    }
}

#[derive(Default)]
pub struct ParallelFt8Decoder {
    pub(crate) filtered_input: bool,
    workers: Vec<Worker>,
}
impl ParallelFt8Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn decode(
        &mut self,
        pcm: &[f32],
        settings: &DecodeSettings,
        messages: &mut MessageDecoder,
        cancel: &AtomicBool,
        on_decode: &mut dyn FnMut(DecodedSignal),
        slot: Option<u64>,
    ) -> Result<DecodeStats, String> {
        if !(1..=12).contains(&settings.threads) {
            return Err("FT8 threads must be within 1..=12".into());
        }
        // Every worker runs the sequential decoder with threads=1. Validate
        // the shared settings once before allocating worker DSP state.
        let mut sequential_settings = settings.clone();
        sequential_settings.threads = 1;
        sequential_settings.validate()?;
        if pcm.len() != 180000
            || pcm.iter().any(|x| {
                !x.is_finite()
                    || x.abs()
                        > if self.filtered_input {
                            MAX_FILTERED_PCM
                        } else {
                            32768.0
                        }
            })
        {
            return Err("FT8 requires 180000 finite PCM samples in [-32768,32768]".into());
        }
        if cancel.load(Ordering::Relaxed) {
            return Ok(DecodeStats {
                cancelled: true,
                ..DecodeStats::default()
            });
        }
        self.workers.resize_with(settings.threads, Worker::new);
        for worker in &mut self.workers {
            worker.decoder.filtered_input = self.filtered_input;
            if let Some(slot) = slot {
                worker.decoder.set_slot(slot);
            }
        }
        if settings.threads == 1 {
            return self.workers[0].decoder.decode(
                pcm,
                &sequential_settings,
                messages,
                cancel,
                on_decode,
            );
        }
        for worker in &mut self.workers {
            worker.messages = messages.clone();
        }
        let bands = partition(settings.low_hz, settings.high_hz, settings.threads);
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(settings.threads);
            for (worker, band) in self.workers.iter_mut().zip(bands) {
                let mut worker_settings = sequential_settings.clone();
                worker_settings.low_hz = (band.low - GUARD_HZ).max(settings.low_hz);
                worker_settings.high_hz = (band.high + GUARD_HZ).min(settings.high_hz);
                handles.push(scope.spawn(move || {
                    let mut found = Vec::new();
                    let mut overflow = false;
                    let stats = worker.decoder.decode(
                        pcm,
                        &worker_settings,
                        &mut worker.messages,
                        cancel,
                        &mut |signal| {
                            // All worker decodes still teach that decoder's list
                            // history and participate in successive subtraction.
                            if band.owns(signal.frequency_hz) {
                                if found.len() < MAX_WORKER_RESULTS {
                                    found.push(signal);
                                } else {
                                    overflow = true;
                                }
                            }
                        },
                    )?;
                    if overflow {
                        return Err("FT8 worker result limit exceeded".to_owned());
                    }
                    Ok((stats, found))
                }));
            }
            // Join every worker even when one fails, avoiding detached work and
            // ensuring all borrowed input is released before this call returns.
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| "FT8 worker panicked".to_owned())
                        .and_then(|r| r)
                })
                .collect::<Vec<_>>()
        });
        let mut stats = DecodeStats::default();
        let mut accepted: Vec<([bool; 77], f32, f32)> = Vec::new();
        for result in results {
            let (worker_stats, found) = result?;
            stats.candidates += worker_stats.candidates;
            stats.passes += worker_stats.passes;
            stats.cancelled |= worker_stats.cancelled;
            for mut signal in found {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                // Refinement can straddle a boundary by a fraction of a hertz;
                // retain only one observation of the same physical signal.
                if accepted.iter().any(|(bits, frequency, time)| {
                    *bits == signal.source_bits
                        && (*frequency - signal.frequency_hz).abs() < 6.25
                        && (*time - signal.dt_seconds).abs() < 0.08
                }) {
                    continue;
                }
                accepted.push((signal.source_bits, signal.frequency_hz, signal.dt_seconds));
                if let Ok(message) = messages.unpack(&signal.source_bits) {
                    signal.message = message;
                }
                stats.decoded += 1;
                on_decode(signal);
            }
        }
        stats.cancelled |= cancel.load(Ordering::Relaxed);
        Ok(stats)
    }
}

fn partition(low: f32, high: f32, threads: usize) -> Vec<Band> {
    let edges: Vec<_> = (0..=threads)
        .map(|i| {
            if i == threads {
                high
            } else {
                low + (high - low) * i as f32 / threads as f32
            }
        })
        .collect();
    edges
        .windows(2)
        .enumerate()
        .map(|(i, e)| Band {
            low: e[0],
            high: e[1],
            last: i + 1 == threads,
        })
        .collect()
}
