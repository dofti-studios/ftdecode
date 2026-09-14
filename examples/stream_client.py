#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Replay a mono 12 kHz s16le WAV through v1 TCP while reading results.

python3 examples/stream_client.py --port 7373 --mode ft8 --operation live tests/fixtures/ft8-clean.wav
The WAV's first sample is a relative slot boundary unless timing is supplied.
--slot-offset uses 12 kHz samples; --start-utc-ns timestamps sample zero.
See docs/protocol.md for commands, events, gaps, and incomplete tails.
Live replay is paced at the sample rate. This example does not capture audio.
"""
import argparse
import json
import socket
import struct
import threading
import time
import wave


def exact(sock, count):
    chunks = bytearray()
    while len(chunks) < count:
        block = sock.recv(count - len(chunks))
        if not block:
            raise EOFError("server closed connection")
        chunks.extend(block)
    return chunks


def receive(sock):
    size, = struct.unpack("<I", exact(sock, 4))
    if not 0 < size <= 8192:
        raise ValueError("invalid server header length")
    header = json.loads(exact(sock, size))
    if header.get("payload_bytes") != 0:
        raise ValueError("unexpected server payload")
    print(json.dumps(header), flush=True)
    return header


def send(sock, header, payload=b""):
    header = dict(header, payload_bytes=len(payload))
    raw = json.dumps(header, separators=(",", ":")).encode()
    if not 0 < len(raw) <= 8192 or len(payload) > 24000:
        raise ValueError("frame exceeds protocol limits")
    sock.sendall(struct.pack("<I", len(raw)) + raw + payload)


def sample_position(text):
    if not text.isascii() or not text.isdecimal() or int(text) > 2**64 - 1:
        raise argparse.ArgumentTypeError("expected a nonnegative decimal u64")
    return int(text)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wav")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--mode", choices=["ft8", "ft4"], required=True)
    parser.add_argument("--operation", choices=["live", "offline"], default="offline")
    timing = parser.add_mutually_exclusive_group()
    timing.add_argument("--slot-offset", type=sample_position, help="first slot boundary in 12 kHz samples; default 0")
    timing.add_argument("--start-utc-ns", type=sample_position, help="UTC timestamp of WAV sample zero, not send time")
    parser.add_argument("--chunk-samples", type=int, default=600, help="1..12000; default 600 (50 ms)")
    parser.add_argument("--settings", type=json.loads, default={}, help='protocol settings JSON, e.g. \'{"ap":"auto"}\'')
    args = parser.parse_args()
    if not 1 <= args.chunk_samples <= 12000:
        parser.error("--chunk-samples must be within 1..12000")
    if not isinstance(args.settings, dict):
        parser.error("--settings must be a JSON object")
    errors = []
    with wave.open(args.wav, "rb") as wav, socket.create_connection(("127.0.0.1", args.port), timeout=5) as sock:
        if (wav.getnchannels(), wav.getsampwidth(), wav.getframerate()) != (1, 2, 12000):
            raise ValueError("WAV must be mono 12 kHz signed 16-bit PCM")
        start = {"type": "start", "version": 1, "mode": args.mode,
                 "operation": args.operation, "settings": args.settings}
        if args.start_utc_ns is not None:
            start["start_utc_ns"] = str(args.start_utc_ns)
        else:
            start["slot_offset_samples"] = str(args.slot_offset or 0)
        send(sock, start)
        acknowledgement = receive(sock)
        if acknowledgement.get("type") != "ok" or acknowledgement.get("command") != "start":
            raise RuntimeError("start rejected")
        # A healthy FT8 stream may produce no output for a whole 15-second slot.
        # Do not retain the short connection/handshake timeout for result reads.
        sock.settimeout(None)

        def shutdown():
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

        def read_results():
            try:
                while True:
                    event = receive(sock)
                    if event.get("type") == "error" or (event.get("type") == "done" and event.get("status") == "failed"):
                        raise RuntimeError(event.get("message", event.get("reason", "protocol error")))
                    if event.get("type") == "ok" and event.get("command") == "finish":
                        return
            except Exception as exc:
                errors.append(exc)
                shutdown()

        receiver = threading.Thread(target=read_results, daemon=True)
        receiver.start()
        index = 0
        origin = time.monotonic()
        try:
            while pcm := wav.readframes(args.chunk_samples):
                count = len(pcm) // 2
                if args.operation == "live":
                    # Simulate delivery after this chunk has been captured.
                    time.sleep(max(0, origin + (index + count) / 12000 - time.monotonic()))
                if errors:
                    raise errors[0]
                send(sock, {"type": "audio", "first_sample": str(index), "sample_count": count}, pcm)
                index += count
            send(sock, {"type": "finish"})
            receiver.join()
        except BaseException:
            shutdown()
            receiver.join(timeout=5)
            raise
        if errors:
            raise errors[0]


if __name__ == "__main__":
    main()
