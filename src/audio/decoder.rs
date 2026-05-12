//! Opus decoder + HID wire-format parser for the Siri Remote
//! microphone stream (HID input report `0xFA`).
//!
//! The remote emits one Opus frame per BLE notification while the
//! Siri button is held; we mirror that 1-frame-per-packet cadence
//! into the shared [`Ring`] for the PipeWire side to consume.

use std::collections::VecDeque;
use std::sync::Mutex;

use anyhow::{Context as _, Result};

use super::{FRAME_SAMPLES, RING_CAPACITY_SAMPLES, SAMPLE_RATE};

/// Total HID input payload length the remote sends for report `0xFA`.
pub const MIC_REPORT_LEN: usize = 99;

/// Maximum Opus frame length the 99-byte HID payload can carry
/// (`99 - 5` header bytes).
pub const MAX_OPUS_FRAME_LEN: usize = MIC_REPORT_LEN - 5;

/// Cap on consecutive packet-loss concealment frames before we stop
/// pretending the stream is continuous and resync to the next real
/// packet. Larger gaps almost always indicate a button release or
/// real BLE disruption rather than a single dropped notification.
pub const MAX_PLC_FRAMES: u16 = 4;

/// Stateful Opus decoder for one Siri Remote audio stream.
pub struct MicDecoder {
    dec: opus::Decoder,
    /// Next expected sequence number; `None` until the first packet
    /// arrives or after a hard resync (button-release sentinel).
    next_seq: Option<u16>,
    /// Pre-sized scratch buffer; libopus always fills `FRAME_SAMPLES`
    /// samples for our 20 ms config so this never reallocates.
    pcm: Vec<i16>,
}

impl MicDecoder {
    pub fn new() -> Result<Self> {
        let dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)
            .context("creating Opus decoder")?;
        Ok(Self {
            dec,
            next_seq: None,
            pcm: vec![0i16; FRAME_SAMPLES],
        })
    }

    /// Decode one HID `0xFA` payload and push the resulting PCM into
    /// `ring`. Sequence-number gaps are filled with up to
    /// [`MAX_PLC_FRAMES`] frames of libopus packet-loss concealment;
    /// all-zero / truncated payloads (the button-released sentinel)
    /// reset sequence tracking so the next press resyncs cleanly.
    pub fn feed(&mut self, payload: &[u8], ring: &Mutex<VecDeque<i16>>) {
        let Some((seq, frame)) = parse_packet(payload) else {
            self.next_seq = None;
            return;
        };

        if let Some(expected) = self.next_seq
            && seq != expected
        {
            let gap = seq.wrapping_sub(expected).min(MAX_PLC_FRAMES);
            for _ in 0..gap {
                if let Ok(samples) = self.dec.decode(&[], &mut self.pcm, false) {
                    push_samples(ring, &self.pcm[..samples]);
                }
            }
        }

        match self.dec.decode(frame, &mut self.pcm, false) {
            Ok(samples) => push_samples(ring, &self.pcm[..samples]),
            Err(err) => log::warn!("opus decode error (seq={seq}, len={}): {err}", frame.len()),
        }

        self.next_seq = Some(seq.wrapping_add(1));
    }

    /// Exposed for tests: peek at the next-expected sequence so we
    /// can assert the button-release sentinel resets the state.
    #[cfg(test)]
    pub(crate) fn next_seq(&self) -> Option<u16> {
        self.next_seq
    }
}

/// Parse one HID `0xFA` payload into `(seq, opus_frame)`.
///
/// Wire layout (99-byte payload):
///   `[0..2]`  uninitialised prefix; the remote zeroes it for every
///             packet except the first two of a button press. Ignored.
///   `[2..4]`  sequence number, `u16` little-endian (wraps at 65535).
///   `[4]`     Opus frame length `L` in bytes.
///   `[5..5+L]` Opus frame (TOC + body).
///   `[5+L..]` zero padding.
///
/// All-zero payloads (button-released sentinel) and packets whose
/// declared frame length would overflow the buffer are rejected.
pub fn parse_packet(payload: &[u8]) -> Option<(u16, &[u8])> {
    if payload.len() < 6 {
        return None;
    }
    let seq = u16::from_le_bytes([payload[2], payload[3]]);
    let len = payload[4] as usize;
    if len == 0 || len > MAX_OPUS_FRAME_LEN || 5 + len > payload.len() {
        return None;
    }
    Some((seq, &payload[5..5 + len]))
}

/// Push samples into the ring, dropping the oldest on overflow so the
/// live stream stays close to real-time.
pub fn push_samples(ring: &Mutex<VecDeque<i16>>, samples: &[i16]) {
    if samples.is_empty() {
        return;
    }
    let mut guard = match ring.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let current = guard.len();
    let overflow = (current + samples.len()).saturating_sub(RING_CAPACITY_SAMPLES);
    if overflow > 0 {
        guard.drain(..overflow.min(current));
    }
    guard.extend(samples.iter().copied());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::new_ring;

    fn parse_hex_line(line: &str) -> Vec<u8> {
        let raw = line.split("raw=").nth(1).expect("dump line has raw=");
        raw.split_whitespace()
            .map(|h| u8::from_str_radix(h, 16).expect("hex byte"))
            .collect()
    }

    fn load_fixture() -> Vec<Vec<u8>> {
        let s = std::fs::read_to_string("microphone-dump.txt").expect("microphone-dump.txt");
        s.lines()
            .filter(|l| !l.is_empty())
            .map(parse_hex_line)
            .collect()
    }

    #[test]
    fn parse_first_fixture_packet() {
        let packets = load_fixture();
        let pkt = &packets[0];
        assert_eq!(pkt.len(), MIC_REPORT_LEN);
        let (seq, frame) = parse_packet(pkt).expect("valid packet");
        assert_eq!(seq, 0);
        let declared_len = pkt[4] as usize;
        assert_eq!(frame.len(), declared_len);
        assert_eq!(frame, &pkt[5..5 + declared_len]);
        // First fixture packet has TOC byte 0xB8 (CELT-only WB 20 ms).
        assert_eq!(frame[0], 0xB8);
    }

    #[test]
    fn parse_rejects_all_zero_payload() {
        let p = vec![0u8; MIC_REPORT_LEN];
        assert!(parse_packet(&p).is_none());
    }

    #[test]
    fn parse_rejects_oversized_len() {
        let mut p = vec![0u8; MIC_REPORT_LEN];
        p[4] = (MAX_OPUS_FRAME_LEN + 1) as u8;
        assert!(parse_packet(&p).is_none());
    }

    #[test]
    fn parse_rejects_short_payload() {
        assert!(parse_packet(&[0u8; 5]).is_none());
    }

    #[test]
    fn decode_full_fixture_yields_one_frame_per_packet() {
        let packets = load_fixture();
        let audio_packets: Vec<&Vec<u8>> = packets
            .iter()
            .filter(|p| parse_packet(p).is_some())
            .collect();
        assert_eq!(audio_packets.len(), 44, "expected 44 decodable packets");

        let ring = new_ring();
        let mut decoder = MicDecoder::new().expect("opus decoder");
        for p in &packets {
            decoder.feed(p, &ring);
        }
        // 44 frames × 960 samples = 42_240 samples produced. The ring
        // is bounded at RING_CAPACITY_SAMPLES (=12_000); after overflow
        // drops we expect exactly the cap to remain.
        let produced = 44 * FRAME_SAMPLES;
        let expected_in_ring = produced.min(RING_CAPACITY_SAMPLES);
        assert_eq!(ring.lock().unwrap().len(), expected_in_ring);

        // Trailing sentinel must have reset sequence tracking.
        assert!(decoder.next_seq().is_none());
    }
}
