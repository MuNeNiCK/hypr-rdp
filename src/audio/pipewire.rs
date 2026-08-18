use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use ironrdp_rdpsnd::server::RdpsndServerMessage;
use ironrdp_server::ServerEvent;
use pipewire::spa::buffer::ChunkFlags;
use tokio::sync::mpsc;

use super::format::{CHANNELS, SAMPLE_RATE};

type SpaAudioFormat = pipewire::spa::param::audio::AudioFormat;

/// User data passed to PipeWire stream callbacks.
struct CaptureData {
    format: pipewire::spa::param::audio::AudioInfoRaw,
    sender: mpsc::UnboundedSender<ServerEvent>,
    stop_signal: Arc<AtomicBool>,
}

/// Run PipeWire audio capture on the current thread (blocking).
pub(super) fn run_capture(
    sender: mpsc::UnboundedSender<ServerEvent>,
    stop_signal: Arc<AtomicBool>,
    startup_tx: Option<std_mpsc::Sender<Result<(), String>>>,
) -> anyhow::Result<()> {
    let mut startup_tx = startup_tx;
    let result = run_capture_inner(sender, stop_signal, &mut startup_tx);
    if let Err(e) = &result {
        report_startup_status(&mut startup_tx, Err(format!("{e:#}")));
    }
    result
}

fn report_startup_status(
    startup_tx: &mut Option<std_mpsc::Sender<Result<(), String>>>,
    status: Result<(), String>,
) {
    if let Some(tx) = startup_tx.take() {
        let _ = tx.send(status);
    }
}

fn run_capture_inner(
    sender: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
    stop_signal: Arc<AtomicBool>,
    startup_tx: &mut Option<std_mpsc::Sender<Result<(), String>>>,
) -> anyhow::Result<()> {
    use pipewire as pw;
    use pw::spa;
    use pw::spa::pod::Pod;

    let mainloop = pw::main_loop::MainLoopBox::new(None)
        .map_err(|_| anyhow::anyhow!("Failed to create PipeWire MainLoop"))?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None)
        .map_err(|_| anyhow::anyhow!("Failed to create PipeWire Context"))?;
    let core = context
        .connect(None)
        .map_err(|_| anyhow::anyhow!("Failed to connect to PipeWire daemon"))?;

    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
        *pw::keys::NODE_NAME => "hypr-rdp-audio",
        *pw::keys::APP_NAME => "hypr-rdp",
        "stream.capture.sink" => "true",
    };

    let stream = pw::stream::StreamBox::new(&core, "hypr-rdp-audio", props)
        .map_err(|_| anyhow::anyhow!("Failed to create PipeWire stream"))?;

    let user_data = CaptureData {
        format: spa::param::audio::AudioInfoRaw::default(),
        sender,
        stop_signal: Arc::clone(&stop_signal),
    };

    let stop_for_state = Arc::clone(&stop_signal);

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(move |_stream, _data, old, new| {
            tracing::trace!("Audio stream state: {:?} -> {:?}", old, new);
            if let pw::stream::StreamState::Error(err) = new {
                tracing::error!("Audio stream error: {}", err);
                stop_for_state.store(true, Ordering::SeqCst);
            }
        })
        .param_changed(|_stream, data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }

            let (media_type, media_subtype) = match spa::param::format_utils::parse_format(param) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("Audio: failed to parse format: {:?}", e);
                    return;
                }
            };

            if media_type != spa::param::format::MediaType::Audio
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }

            if let Err(e) = data.format.parse(param) {
                tracing::warn!("Audio: failed to parse audio info: {:?}", e);
                return;
            }

            let rate = data.format.rate();
            let channels = data.format.channels();
            let format = data.format.format();
            tracing::trace!(
                "Audio format negotiated: rate={}, channels={}, format={:?}",
                rate, channels, format
            );

            // Validate against the format advertised to the RDP client
            if rate != SAMPLE_RATE || channels != CHANNELS as u32 {
                tracing::warn!(
                    "Audio format mismatch: expected {}Hz {}ch, got {}Hz {}ch — audio may be corrupted",
                    SAMPLE_RATE, CHANNELS, rate, channels
                );
            }
        })
        .process(|stream, data| {
            if data.stop_signal.load(Ordering::Relaxed) {
                return;
            }

            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };

            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }

            let chunk = datas[0].chunk();
            let offset = chunk.offset();
            let size = chunk.size();
            let flags = chunk.flags();

            let Some(slice) = datas[0].data() else {
                return;
            };

            let Some(payload) = chunk_payload(slice, offset, size, flags) else {
                return;
            };

            let captured_at_ms = match system_uptime_millis() {
                Ok(timestamp) => timestamp,
                Err(error) => {
                    tracing::error!(%error, "Audio: failed to read system uptime");
                    return;
                }
            };

            let Some(pcm_bytes) = convert_to_s16le(data.format.format(), payload.as_ref(), payload.len()) else {
                return;
            };

            emit_wave_chunk(&data.sender, captured_at_ms, pcm_bytes);
        })
        .register()
        .map_err(|_| anyhow::anyhow!("Failed to register stream listener"))?;

    // Build audio format pod: request S16LE
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(CHANNELS as u32);

    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };

    let pod_bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow::anyhow!("Failed to serialize audio format pod: {:?}", e))?
    .0
    .into_inner();

    let pod = Pod::from_bytes(&pod_bytes)
        .ok_or_else(|| anyhow::anyhow!("Failed to create pod from bytes"))?;

    let flags = pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS;

    stream
        .connect(spa::utils::Direction::Input, None, flags, &mut [pod])
        .map_err(|_| anyhow::anyhow!("Failed to connect PipeWire stream"))?;

    report_startup_status(startup_tx, Ok(()));
    tracing::trace!("Audio: PipeWire stream connected, entering main loop");

    let loop_ref = mainloop.loop_();
    while !stop_signal.load(Ordering::Relaxed) {
        loop_ref.iterate(std::time::Duration::from_millis(100));
    }

    tracing::trace!("Audio: PipeWire capture stopped");
    Ok(())
}

fn chunk_payload(input: &[u8], offset: u32, size: u32, flags: ChunkFlags) -> Option<Cow<'_, [u8]>> {
    if input.is_empty() || size == 0 || flags.contains(ChunkFlags::CORRUPTED) {
        return None;
    }

    let byte_count = (size as usize).min(input.len());
    let start = (offset as usize) % input.len();
    let end = start + byte_count;

    if end <= input.len() {
        Some(Cow::Borrowed(&input[start..end]))
    } else {
        let first_len = input.len() - start;
        let second_len = byte_count - first_len;
        let mut out = Vec::with_capacity(byte_count);
        out.extend_from_slice(&input[start..]);
        out.extend_from_slice(&input[..second_len]);
        Some(Cow::Owned(out))
    }
}

fn convert_to_s16le(format: SpaAudioFormat, input: &[u8], byte_count: usize) -> Option<Vec<u8>> {
    let byte_count = byte_count.min(input.len());
    let pcm_bytes = match format {
        SpaAudioFormat::S16LE => input[..byte_count].to_vec(),
        SpaAudioFormat::S16BE => {
            let mut out = Vec::with_capacity(byte_count);
            for pair in input[..byte_count].chunks_exact(2) {
                out.push(pair[1]);
                out.push(pair[0]);
            }
            out
        }
        SpaAudioFormat::F32LE | SpaAudioFormat::F32BE => {
            let mut out = Vec::with_capacity((byte_count / 4) * 2);
            for chunk in input[..byte_count].chunks_exact(4) {
                let bytes: [u8; 4] = [chunk[0], chunk[1], chunk[2], chunk[3]];
                let sample = if format == SpaAudioFormat::F32LE {
                    f32::from_le_bytes(bytes)
                } else {
                    f32::from_be_bytes(bytes)
                };
                let s16 = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
                out.extend_from_slice(&s16.to_le_bytes());
            }
            out
        }
        _ => return None,
    };

    (!pcm_bytes.is_empty()).then_some(pcm_bytes)
}

/// Return the number of milliseconds elapsed since system boot.
fn system_uptime_millis() -> std::io::Result<u64> {
    system_uptime_millis_with(|clock_id| {
        let mut timestamp = std::mem::MaybeUninit::<libc::timespec>::uninit();

        // SAFETY: clock_gettime initializes the pointed-to timespec on success.
        if unsafe { libc::clock_gettime(clock_id, timestamp.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: the successful clock_gettime call above initialized timestamp.
        Ok(unsafe { timestamp.assume_init() })
    })
}

fn system_uptime_millis_with(
    read_clock: impl FnOnce(libc::clockid_t) -> std::io::Result<libc::timespec>,
) -> std::io::Result<u64> {
    let timestamp = read_clock(libc::CLOCK_BOOTTIME)?;
    if timestamp.tv_sec < 0 || !(0..1_000_000_000).contains(&timestamp.tv_nsec) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "CLOCK_BOOTTIME returned an invalid timespec",
        ));
    }

    let seconds_ms = (timestamp.tv_sec as u64)
        .checked_mul(1000)
        .ok_or_else(|| std::io::Error::other("system uptime exceeds u64 milliseconds"))?;
    seconds_ms
        .checked_add((timestamp.tv_nsec as u64) / 1_000_000)
        .ok_or_else(|| std::io::Error::other("system uptime exceeds u64 milliseconds"))
}

/// MS-RDPEA 2.2.3.10 defines dwAudioTimeStamp as the wrapped 32-bit number of
/// milliseconds since system start when the server obtains the audio data.
fn emit_wave_chunk(
    sender: &mpsc::UnboundedSender<ServerEvent>,
    captured_at_ms: u64,
    pcm_bytes: Vec<u8>,
) {
    if pcm_bytes.is_empty() {
        return;
    }

    let _ = sender.send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(
        pcm_bytes,
        captured_at_ms as u32,
    )));
}

#[cfg(test)]
mod tests {
    use ironrdp_rdpsnd::server::RdpsndServerMessage;
    use pipewire::spa::buffer::ChunkFlags;

    use crate::audio::format::BLOCK_ALIGN;

    use super::*;

    fn f32le_bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn f32be_bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    #[test]
    fn pipewire_chunk_payload_uses_offset_size_and_wraps_at_buffer_end() {
        let input = [0, 1, 2, 3, 4, 5];

        assert_eq!(
            chunk_payload(&input, 2, 3, ChunkFlags::empty())
                .unwrap()
                .as_ref(),
            &[2, 3, 4]
        );
        assert_eq!(
            chunk_payload(&input, 8, 2, ChunkFlags::empty())
                .unwrap()
                .as_ref(),
            &[2, 3]
        );
        assert_eq!(
            chunk_payload(&input, 4, 4, ChunkFlags::empty())
                .unwrap()
                .as_ref(),
            &[4, 5, 0, 1]
        );
        assert_eq!(
            chunk_payload(&input, 0, 99, ChunkFlags::empty())
                .unwrap()
                .as_ref(),
            &input
        );
        assert!(chunk_payload(&input, 0, 0, ChunkFlags::empty()).is_none());
        assert!(chunk_payload(&[], 0, 1, ChunkFlags::empty()).is_none());
    }

    #[test]
    fn pipewire_corrupted_chunk_is_not_forwarded() {
        let input = [1, 2, 3, 4];

        assert!(chunk_payload(&input, 0, 4, ChunkFlags::CORRUPTED).is_none());
    }

    #[test]
    fn convert_to_s16le_passes_through_little_endian_pcm() {
        let input = [0x34, 0x12, 0x78, 0x56, 0xff];

        let converted = convert_to_s16le(SpaAudioFormat::S16LE, &input, 4).unwrap();

        assert_eq!(converted, vec![0x34, 0x12, 0x78, 0x56]);
    }

    #[test]
    fn convert_to_s16le_swaps_big_endian_pcm_and_drops_trailing_byte() {
        let input = [0x12, 0x34, 0x56, 0x78, 0xff];

        let converted = convert_to_s16le(SpaAudioFormat::S16BE, &input, input.len()).unwrap();

        assert_eq!(converted, vec![0x34, 0x12, 0x78, 0x56]);
    }

    #[test]
    fn convert_to_s16le_converts_and_clamps_float_samples() {
        let converted = convert_to_s16le(
            SpaAudioFormat::F32LE,
            &f32le_bytes(&[-2.0, -0.5, 0.0, 0.5, 2.0]),
            20,
        )
        .unwrap();

        let samples: Vec<i16> = converted
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        assert_eq!(samples, vec![-32767, -16383, 0, 16383, 32767]);
    }

    #[test]
    fn convert_to_s16le_reads_big_endian_float_samples() {
        let converted = convert_to_s16le(SpaAudioFormat::F32BE, &f32be_bytes(&[1.0]), 4).unwrap();

        assert_eq!(converted, 32767i16.to_le_bytes());
    }

    #[test]
    fn convert_to_s16le_rejects_unsupported_or_empty_input() {
        assert!(convert_to_s16le(SpaAudioFormat::U8, &[0x80], 1).is_none());
        assert!(convert_to_s16le(SpaAudioFormat::S16LE, &[], 0).is_none());
        assert!(convert_to_s16le(SpaAudioFormat::F32LE, &[0x00, 0x00, 0x00], 3).is_none());
    }

    #[test]
    fn emit_wave_chunk_forwards_capture_uptime_milliseconds() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let captured_at_ms = 123_456;

        emit_wave_chunk(&sender, captured_at_ms, vec![1, 2, 3, 4, 5, 6, 7, 8]);

        match receiver.try_recv().unwrap() {
            ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, ts)) => {
                assert_eq!(data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
                assert_eq!(ts, 123_456);
            }
            other => panic!("unexpected server event: {other:?}"),
        }
    }

    #[test]
    fn emit_wave_chunk_wraps_capture_uptime_to_wire_width() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let captured_at_ms = u64::from(u32::MAX) + 42;

        emit_wave_chunk(&sender, captured_at_ms, vec![0; BLOCK_ALIGN as usize]);

        match receiver.try_recv().unwrap() {
            ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(_, ts)) => assert_eq!(ts, 41),
            other => panic!("unexpected server event: {other:?}"),
        }
    }

    #[test]
    fn system_uptime_millis_uses_boottime_and_converts_nanoseconds() {
        let timestamp = system_uptime_millis_with(|clock_id| {
            assert_eq!(clock_id, libc::CLOCK_BOOTTIME);
            Ok(libc::timespec {
                tv_sec: 123,
                tv_nsec: 456_789_012,
            })
        })
        .unwrap();

        assert_eq!(timestamp, 123_456);
    }

    #[test]
    fn system_uptime_millis_rejects_invalid_clock_values() {
        let error = system_uptime_millis_with(|_| {
            Ok(libc::timespec {
                tv_sec: -1,
                tv_nsec: 0,
            })
        })
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn emit_wave_chunk_drops_empty_input() {
        let (sender, mut receiver) = mpsc::unbounded_channel();

        emit_wave_chunk(&sender, 7, Vec::new());

        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn emit_wave_chunk_ignores_closed_receiver() {
        let (sender, receiver) = mpsc::unbounded_channel();
        drop(receiver);

        emit_wave_chunk(&sender, 41, vec![1, 2, 3, 4]);
    }
}
