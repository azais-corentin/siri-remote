//! Audio plumbing for `siri-remote mic`: Opus decoding + PipeWire
//! source exposure.
//!
//! The two sub-modules are kept narrowly focused per the plan's
//! single-responsibility split:
//!
//! - [`decoder`] — turns one HID `0xFA` payload into 20 ms of mono
//!   S16LE PCM via libopus, with bounded packet-loss concealment on
//!   sequence-number gaps.
//! - [`pipewire`] — owns the PipeWire main-loop OS thread and drains
//!   decoded samples into the audio graph as an `Audio/Source` node.
//!
//! Both halves talk over the same `Ring` type (a bounded
//! `VecDeque<i16>` behind a `Mutex`) so the live stream stays close
//! to real-time even if a consumer is slow.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub mod decoder;
pub mod pipewire;

pub use decoder::MicDecoder;
pub use pipewire::PipeWireWorker;

/// Opus canonical output rate. PipeWire's graph resampler handles any
/// consumer-side rate negotiation, so we never need to vary this.
pub const SAMPLE_RATE: u32 = 48_000;

/// Mono S16LE → 2 bytes per sample on the wire.
pub const BYTES_PER_SAMPLE: usize = 2;

/// 20 ms frame @ 48 kHz — matches the Opus CELT-only WB config
/// (`config=23`) the Siri Remote uses (TOC byte `0xB8`; see the
/// mic plan).
pub const FRAME_SAMPLES: usize = 960;

/// Bound the bridge ring buffer at ~250 ms of audio. When the BLE
/// producer outruns the PipeWire consumer we drop oldest samples so
/// the live stream stays close to real-time (the right tradeoff for
/// voice assistants and anything that wants the freshest speech).
pub const RING_CAPACITY_SAMPLES: usize = SAMPLE_RATE as usize / 4;

/// Shared SPSC-style ring connecting the BLE decoder (producer) to the
/// PipeWire `process()` callback (consumer). The mutex is uncontended
/// in steady state; both sides hold it for only the time required to
/// drain or extend the deque.
pub type Ring = Arc<Mutex<VecDeque<i16>>>;

/// Allocate a fresh `Ring` pre-sized to its overflow cap.
pub fn new_ring() -> Ring {
    Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY_SAMPLES)))
}
