use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ironrdp_rdpsnd::server::RdpsndServerMessage;
use ironrdp_server::ServerEvent;
use tokio::sync::mpsc;

use super::format::{BLOCK_ALIGN, CHANNELS, SAMPLE_RATE};

/// User data passed to PipeWire stream callbacks.
struct CaptureData {
    format: pipewire::spa::param::audio::AudioInfoRaw,
    sender: mpsc::UnboundedSender<ServerEvent>,
    stop_signal: Arc<AtomicBool>,
    timestamp: u32,
}

/// Run PipeWire audio capture on the current thread (blocking).
pub(super) fn run_capture(
    sender: mpsc::UnboundedSender<ServerEvent>,
    stop_signal: Arc<AtomicBool>,
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
        timestamp: 0,
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
            let size = chunk.size() as usize;
            if size == 0 {
                return;
            }

            let Some(slice) = datas[0].data() else {
                return;
            };

            let byte_count = size.min(slice.len());

            // Convert captured audio to S16LE bytes for RDP
            let pcm_bytes: Vec<u8> = match data.format.format() {
                spa::param::audio::AudioFormat::S16LE => {
                    slice[..byte_count].to_vec()
                }
                spa::param::audio::AudioFormat::S16BE => {
                    // Swap bytes for each sample
                    let mut out = Vec::with_capacity(byte_count);
                    for pair in slice[..byte_count].chunks_exact(2) {
                        out.push(pair[1]);
                        out.push(pair[0]);
                    }
                    out
                }
                spa::param::audio::AudioFormat::F32LE | spa::param::audio::AudioFormat::F32BE => {
                    let sample_count = byte_count / 4;
                    let mut out = Vec::with_capacity(sample_count * 2);
                    for i in 0..sample_count {
                        let start = i * 4;
                        if start + 4 > slice.len() {
                            break;
                        }
                        let bytes: [u8; 4] = [
                            slice[start], slice[start + 1], slice[start + 2], slice[start + 3],
                        ];
                        let f = if data.format.format() == spa::param::audio::AudioFormat::F32LE {
                            f32::from_le_bytes(bytes)
                        } else {
                            f32::from_be_bytes(bytes)
                        };
                        let s16 = (f.clamp(-1.0, 1.0) * 32767.0) as i16;
                        out.extend_from_slice(&s16.to_le_bytes());
                    }
                    out
                }
                _ => return,
            };

            if pcm_bytes.is_empty() {
                return;
            }

            let samples = pcm_bytes.len() / (BLOCK_ALIGN as usize);
            let _ = data.sender.send(ServerEvent::Rdpsnd(
                RdpsndServerMessage::Wave(pcm_bytes, data.timestamp),
            ));
            data.timestamp = data.timestamp.wrapping_add(samples as u32);
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

    tracing::trace!("Audio: PipeWire stream connected, entering main loop");

    let loop_ref = mainloop.loop_();
    while !stop_signal.load(Ordering::Relaxed) {
        loop_ref.iterate(std::time::Duration::from_millis(100));
    }

    tracing::trace!("Audio: PipeWire capture stopped");
    Ok(())
}
