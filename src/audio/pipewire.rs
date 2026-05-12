//! PipeWire side of the `siri-remote mic` bridge.
//!
//! PipeWire's main loop is `!Send`, so we own its OS thread end-to-end:
//! the loop, the stream, the listener, and the callback's `&mut Ring`
//! all live on that thread. The BLE side talks to us through:
//!
//! - the shared [`Ring`] (decoded PCM samples), and
//! - a [`pw::channel::Sender<()>`] used to ask the loop to quit.

use std::io::Cursor;
use std::sync::{mpsc as std_mpsc, Arc};
use std::thread::{self, JoinHandle};

use anyhow::{Context as _, Result};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::pod::Pod;

use super::{new_ring, Ring, BYTES_PER_SAMPLE, SAMPLE_RATE};

/// Owns the PipeWire main-loop thread and the shared audio ring.
/// Dropping the worker tells the thread to quit and joins it.
pub struct PipeWireWorker {
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    ring: Ring,
}

impl PipeWireWorker {
    /// Spawn the worker thread and wait until the PipeWire stream is
    /// successfully connected before returning. If PipeWire init fails
    /// the thread is joined and the error is propagated so the caller
    /// can fail the subcommand instead of silently streaming into a
    /// node nobody can hear.
    pub fn spawn(node_name: String, node_description: String) -> Result<Self> {
        let ring = new_ring();
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (init_tx, init_rx) = std_mpsc::sync_channel::<Result<(), String>>(1);

        let ring_for_thread = Arc::clone(&ring);
        let thread = thread::Builder::new()
            .name("siri-mic-pipewire".into())
            .spawn(move || {
                if let Err(err) = pw_thread_main(
                    quit_rx,
                    ring_for_thread,
                    node_name,
                    node_description,
                    &init_tx,
                ) {
                    let _ = init_tx.send(Err(format!("{err:?}")));
                }
            })
            .context("spawning siri-mic-pipewire thread")?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                quit: Some(quit_tx),
                thread: Some(thread),
                ring,
            }),
            Ok(Err(msg)) => {
                let _ = thread.join();
                Err(anyhow::anyhow!("PipeWire init failed: {msg}"))
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow::anyhow!(
                    "PipeWire worker thread exited before reporting init status"
                ))
            }
        }
    }

    /// Producer-side handle to the shared sample ring. Clone for each
    /// owner; cheap (just an `Arc` bump).
    pub fn ring(&self) -> Ring {
        Arc::clone(&self.ring)
    }
}

impl Drop for PipeWireWorker {
    fn drop(&mut self) {
        if let Some(tx) = self.quit.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Body of the dedicated PipeWire thread.
///
/// `init_tx` is signalled exactly once: `Ok(())` after the stream is
/// successfully connected, or `Err(_)` (via the spawn wrapper) if
/// anything before that fails. Once signalled we hand control to the
/// PipeWire main loop and return when the quit channel fires.
fn pw_thread_main(
    quit_rx: pw::channel::Receiver<()>,
    ring: Ring,
    node_name: String,
    node_description: String,
    init_tx: &std_mpsc::SyncSender<Result<(), String>>,
) -> Result<()> {
    pw::init();

    let main_loop =
        pw::main_loop::MainLoopRc::new(None).context("creating PipeWire main loop")?;
    let context =
        pw::context::ContextRc::new(&main_loop, None).context("creating PipeWire context")?;
    let core = context
        .connect_rc(None)
        .context("connecting to PipeWire core")?;

    // Quit on shutdown signal. Dropping the sender on the BLE side
    // also drops the receiver pipe; attaching here ensures we exit
    // the main loop cleanly instead of blocking forever.
    let _attached = quit_rx.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        move |_| main_loop.quit()
    });

    let stream = pw::stream::StreamBox::new(
        &core,
        "siri-remote-mic",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::NODE_NAME => node_name.as_str(),
            *pw::keys::NODE_DESCRIPTION => node_description.as_str(),
            *pw::keys::AUDIO_CHANNELS => "1",
        },
    )
    .context("creating PipeWire stream")?;

    let _listener = stream
        .add_local_listener_with_user_data(Arc::clone(&ring))
        .process(fill_buffer)
        .register()
        .context("registering PipeWire stream listener")?;

    let format_params = build_format_param().context("serialising audio format param")?;
    let mut params = [Pod::from_bytes(&format_params).context("decoding serialised format pod")?];

    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .context("connecting PipeWire stream")?;

    // Init complete; let the BLE side start pushing samples. Keep the
    // stream and listener alive on the stack across `run()` — they
    // borrow `core` and the listener's `process` callback uses the
    // ring.
    let _ = init_tx.send(Ok(()));
    main_loop.run();
    drop(stream);
    Ok(())
}

/// Build the SPA pod describing our stream format: mono S16LE @ 48 kHz.
fn build_format_param() -> Result<Vec<u8>> {
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(1);
    let mut position = [0u32; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    audio_info.set_position(position);

    let bytes = spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .map_err(|e| anyhow::anyhow!("pod serialisation failed: {e}"))?
    .0
    .into_inner();
    Ok(bytes)
}

/// PipeWire `process()` callback: drain as much as fits from the ring
/// into the buffer PipeWire just handed us, zero-pad the rest so
/// downstream consumers see continuous audio rather than xruns when
/// the Siri button isn't held.
fn fill_buffer(stream: &pw::stream::Stream, ring: &mut Ring) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    if datas.is_empty() {
        return;
    }
    let slot = &mut datas[0];
    let Some(bytes) = slot.data() else {
        return;
    };
    let cap_samples = bytes.len() / BYTES_PER_SAMPLE;
    let usable_bytes = cap_samples * BYTES_PER_SAMPLE;

    let written_samples = {
        let mut guard = match ring.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let take = cap_samples.min(guard.len());
        let mut written = 0usize;
        let (front, back) = guard.as_slices();
        for src in [front, back] {
            if written >= take {
                break;
            }
            let n = src.len().min(take - written);
            for (i, s) in src[..n].iter().enumerate() {
                let dst_off = (written + i) * BYTES_PER_SAMPLE;
                bytes[dst_off..dst_off + BYTES_PER_SAMPLE].copy_from_slice(&s.to_le_bytes());
            }
            written += n;
        }
        guard.drain(..take);
        written
    };

    // Zero-fill the remainder so the chunk is always exactly
    // `usable_bytes` long — keeps consumer clocks happy when the
    // remote isn't talking.
    for byte in &mut bytes[written_samples * BYTES_PER_SAMPLE..usable_bytes] {
        *byte = 0;
    }

    let chunk = slot.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = BYTES_PER_SAMPLE as i32;
    *chunk.size_mut() = usable_bytes as u32;
}
