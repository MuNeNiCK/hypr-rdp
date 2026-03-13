//! Audio redirection via PipeWire and ironrdp-rdpsnd.
//!
//! Captures system audio using `pw-cat` and sends it over the RDP audio channel.
//! The capture subprocess is started when the client negotiates audio and stopped
//! when the session ends.

use std::fmt;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::thread;

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{RdpsndServerHandler, RdpsndServerMessage};
use ironrdp_server::{ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc;

/// PCM format: 16-bit signed LE, stereo, 44100 Hz
const SAMPLE_RATE: u32 = 44100;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;
const BLOCK_ALIGN: u16 = CHANNELS * (BITS_PER_SAMPLE / 8);

pub struct HyprSoundFactory {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl HyprSoundFactory {
    pub fn new() -> Self {
        Self {
            event_sender: None,
        }
    }
}

impl ServerEventSender for HyprSoundFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.event_sender = Some(sender);
    }
}

impl SoundServerFactory for HyprSoundFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(HyprSoundHandler {
            event_sender: self.event_sender.clone(),
            capture_process: None,
            capture_thread: None,
            formats: vec![AudioFormat {
                format: WaveFormat::PCM,
                n_channels: CHANNELS,
                n_samples_per_sec: SAMPLE_RATE,
                n_avg_bytes_per_sec: SAMPLE_RATE * BLOCK_ALIGN as u32,
                n_block_align: BLOCK_ALIGN,
                bits_per_sample: BITS_PER_SAMPLE,
                data: None,
            }],
        })
    }
}

struct HyprSoundHandler {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    capture_process: Option<Child>,
    capture_thread: Option<thread::JoinHandle<()>>,
    formats: Vec<AudioFormat>,
}

impl fmt::Debug for HyprSoundHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyprSoundHandler")
            .field("capturing", &self.capture_process.is_some())
            .finish()
    }
}

impl RdpsndServerHandler for HyprSoundHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn start(&mut self, client_format: &ironrdp_rdpsnd::pdu::ClientAudioFormatPdu) -> Option<u16> {
        tracing::info!(
            client_formats = client_format.formats.len(),
            "Audio: starting capture ({}Hz, {}ch, {}bit)",
            SAMPLE_RATE, CHANNELS, BITS_PER_SAMPLE
        );

        // Find our PCM format in the client's format list
        let our_format = &self.formats[0];
        let client_format_index = client_format.formats.iter().position(|f| {
            f.format == our_format.format
                && f.n_channels == our_format.n_channels
                && f.n_samples_per_sec == our_format.n_samples_per_sec
                && f.bits_per_sample == our_format.bits_per_sample
        });
        let client_format_index = match client_format_index {
            Some(idx) => idx as u16,
            None => {
                tracing::warn!("Audio: client does not support our PCM format, audio disabled");
                return None;
            }
        };

        // Check if pw-cat is available
        if Command::new("pw-cat").arg("--version").output().is_err() {
            tracing::warn!("Audio: pw-cat not found, audio capture disabled");
            return None;
        }

        // Start PipeWire capture: record system audio as raw PCM
        let child = match Command::new("pw-cat")
            .args([
                "--record",
                "--target", "0",  // Default audio sink monitor
                "--format", "s16",
                "--rate", &SAMPLE_RATE.to_string(),
                "--channels", &CHANNELS.to_string(),
                "-",  // Output to stdout
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!("Audio: failed to start pw-cat: {}", e);
                return None;
            }
        };

        let pid = child.id();
        self.capture_process = Some(child);

        // Spawn reader thread to forward audio data
        if let Some(ref sender) = self.event_sender {
            let sender = sender.clone();
            let mut stdout = self.capture_process.as_mut().unwrap().stdout.take().unwrap();

            match thread::Builder::new()
                .name("audio-capture".into())
                .spawn(move || {
                    // Read in chunks matching RDP audio frame size (~20ms of audio)
                    let chunk_size = (SAMPLE_RATE as usize * BLOCK_ALIGN as usize) / 50; // 20ms
                    let mut buf = vec![0u8; chunk_size];
                    let mut timestamp: u32 = 0;

                    while let Ok(()) = stdout.read_exact(&mut buf) {
                        let _ = sender.send(ServerEvent::Rdpsnd(
                            RdpsndServerMessage::Wave(buf.clone(), timestamp),
                        ));
                        let samples = chunk_size / (BLOCK_ALIGN as usize);
                        timestamp = timestamp.wrapping_add(samples as u32);
                    }
                }) {
                Ok(handle) => {
                    self.capture_thread = Some(handle);
                }
                Err(e) => {
                    tracing::error!("Audio: failed to spawn capture thread: {}", e);
                    if let Some(mut child) = self.capture_process.take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    return None;
                }
            }
        }

        tracing::info!(pid, client_format_index, "Audio: pw-cat capture started");
        Some(client_format_index)
    }

    fn stop(&mut self) {
        tracing::info!("Audio: stopping capture");

        if let Some(mut child) = self.capture_process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        // Thread will exit when stdout is closed
        if let Some(handle) = self.capture_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for HyprSoundHandler {
    fn drop(&mut self) {
        self.stop();
    }
}
