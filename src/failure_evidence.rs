//! Durable record of a pipeline failure.
//!
//! A capture-thread failure reaches the journal only when a service manager
//! owns the process; a process started by hand, or the child of another tool,
//! loses its stdout. The failure that motivated this module left no line at
//! all. Every block is appended to `$XDG_STATE_HOME/hypr-rdp/failure-evidence.log`
//! and flushed before returning, so the record outlives the process.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// A file that grows forever is a disk leak. 256 KiB still holds hundreds of
/// blocks, more than one investigation needs.
const MAX_EVIDENCE_BYTES: u64 = 256 * 1024;

const EVIDENCE_FILE_NAME: &str = "failure-evidence.log";
const EVIDENCE_DIR_NAME: &str = "hypr-rdp";

/// Raw VA status and the operation it belongs to, kept verbatim: a translated
/// message ("invalid parameter") loses the number a header lookup resolves.
pub(crate) struct VaStatusEvidence {
    pub(crate) status: u32,
    pub(crate) operation: &'static str,
    pub(crate) surface: u32,
}

/// Memory-type mask a driver advertised and the import path it selected.
pub(crate) struct ImportEvidence {
    pub(crate) memory_types: Option<u32>,
    pub(crate) import_path: &'static str,
}

pub(crate) struct CaptureBufferEvidence {
    pub(crate) fd: i32,
    pub(crate) stride: u32,
    pub(crate) offset: u32,
    pub(crate) modifier: u64,
    pub(crate) format: u32,
}

pub(crate) struct Nv12ExportEvidence {
    pub(crate) stride: u32,
    pub(crate) offset: u32,
    pub(crate) uv_stride: u32,
    pub(crate) uv_offset: u32,
    pub(crate) modifier: u64,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct FrameCounters {
    pub(crate) captured: u64,
    pub(crate) converted: u64,
    pub(crate) encoded: u64,
    pub(crate) sent: u64,
    pub(crate) skipped: u64,
    pub(crate) failures: u32,
}

pub(crate) struct TransportEvidence {
    pub(crate) in_flight: u32,
    pub(crate) client_queue_depth: u32,
    pub(crate) ack_suspended: bool,
}

pub(crate) struct BehaviourConfig {
    pub(crate) fps: u32,
    pub(crate) resolution: (u32, u32),
    pub(crate) bitrate: u32,
    pub(crate) quality: u8,
    pub(crate) max_frames_in_flight: u32,
    pub(crate) capture_mode: &'static str,
    pub(crate) codec: &'static str,
}

pub(crate) struct FailureEvidence {
    /// Which wait expired, and at which stage.
    pub(crate) attempt: String,
    pub(crate) va_status: Option<VaStatusEvidence>,
    pub(crate) vpp_input: ImportEvidence,
    pub(crate) encoder_input: ImportEvidence,
    pub(crate) capture_buffers: Vec<CaptureBufferEvidence>,
    pub(crate) nv12_export: Nv12ExportEvidence,
    pub(crate) counters: FrameCounters,
    pub(crate) transport: TransportEvidence,
    pub(crate) config: BehaviourConfig,
}

impl FailureEvidence {
    fn block(&self, timestamp: &str, pid: u32) -> String {
        let mut block = String::new();
        block.push_str("=== hypr-rdp failure ===\n");
        block.push_str(&format!("utc: {timestamp}\n"));
        block.push_str(&format!("pid: {pid}\n"));
        block.push_str(&format!("attempt: {}\n", self.attempt));
        match &self.va_status {
            Some(status) => block.push_str(&format!(
                "va_status: {} (operation: {}, surface: {})\n",
                status.status, status.operation, status.surface
            )),
            None => block.push_str("va_status: none\n"),
        }
        block.push_str(&format!(
            "vpp_input: memory_types={} import_path={}\n",
            memory_types(self.vpp_input.memory_types),
            self.vpp_input.import_path
        ));
        block.push_str(&format!(
            "encoder_input: memory_types={} import_path={}\n",
            memory_types(self.encoder_input.memory_types),
            self.encoder_input.import_path
        ));
        for (index, buffer) in self.capture_buffers.iter().enumerate() {
            block.push_str(&format!(
                "capture_buffer[{index}]: fd={} stride={} offset={} modifier=0x{:016x} format=0x{:08x}\n",
                buffer.fd, buffer.stride, buffer.offset, buffer.modifier, buffer.format
            ));
        }
        block.push_str(&format!(
            "nv12_export: stride={} offset={} uv_stride={} uv_offset={} modifier=0x{:016x}\n",
            self.nv12_export.stride,
            self.nv12_export.offset,
            self.nv12_export.uv_stride,
            self.nv12_export.uv_offset,
            self.nv12_export.modifier
        ));
        block.push_str(&format!(
            "counters: captured={} converted={} encoded={} sent={} skipped={} failures={}\n",
            self.counters.captured,
            self.counters.converted,
            self.counters.encoded,
            self.counters.sent,
            self.counters.skipped,
            self.counters.failures
        ));
        block.push_str(&format!(
            "transport: in_flight={} client_queue_depth={} ack_suspended={}\n",
            self.transport.in_flight,
            self.transport.client_queue_depth,
            self.transport.ack_suspended
        ));
        block.push_str(&format!(
            "config: fps={} resolution={}x{} bitrate={} quality={} max_frames_in_flight={} capture_mode={} codec={}\n",
            self.config.fps,
            self.config.resolution.0,
            self.config.resolution.1,
            self.config.bitrate,
            self.config.quality,
            self.config.max_frames_in_flight,
            self.config.capture_mode,
            self.config.codec
        ));
        block.push_str("=== end ===\n");
        block
    }
}

fn memory_types(mask: Option<u32>) -> String {
    mask.map_or_else(|| "query-failed".to_owned(), |mask| format!("0x{mask:08x}"))
}

/// Append one block and flush it before returning.
pub(crate) fn record(evidence: &FailureEvidence) -> std::io::Result<PathBuf> {
    let path = evidence_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let block = evidence.block(&utc_timestamp(SystemTime::now()), std::process::id());
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&path)?;
    file.write_all(block.as_bytes())?;
    file.flush()?;
    truncate_to_tail(&mut file)?;
    Ok(path)
}

fn evidence_path() -> std::io::Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        Some(state_home) => PathBuf::from(state_home),
        None => match std::env::var_os("HOME").filter(|value| !value.is_empty()) {
            Some(home) => PathBuf::from(home).join(".local").join("state"),
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "neither XDG_STATE_HOME nor HOME is set",
                ))
            }
        },
    };
    Ok(base.join(EVIDENCE_DIR_NAME).join(EVIDENCE_FILE_NAME))
}

fn truncate_to_tail(file: &mut File) -> std::io::Result<()> {
    let len = file.metadata()?.len();
    if len <= MAX_EVIDENCE_BYTES {
        return Ok(());
    }

    file.seek(SeekFrom::Start(len - MAX_EVIDENCE_BYTES))?;
    let mut tail = Vec::with_capacity(MAX_EVIDENCE_BYTES as usize);
    file.read_to_end(&mut tail)?;
    let start = start_of_next_line(&tail);
    let tail = &tail[start..];

    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(tail)?;
    file.flush()
}

/// Index of the first full line in a buffer that may begin mid-line, so the
/// truncated file never starts with half a record.
fn start_of_next_line(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1)
}

fn utc_timestamp(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let days = (seconds / 86_400) as i64;
    let time_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60
    )
}

/// Gregorian date for a count of days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`), keeping the module free of a date-time dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = (days - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(va_status: Option<VaStatusEvidence>) -> FailureEvidence {
        FailureEvidence {
            attempt: "vaQuerySurfaceStatus poll on the VPP output surface".into(),
            va_status,
            vpp_input: ImportEvidence {
                memory_types: Some(0x3000_0001),
                import_path: "DRM_PRIME (legacy)",
            },
            encoder_input: ImportEvidence {
                memory_types: None,
                import_path: "not initialized",
            },
            capture_buffers: vec![CaptureBufferEvidence {
                fd: 12,
                stride: 7680,
                offset: 0,
                modifier: 0x0100_0000_0000_0001,
                format: 0x34325258,
            }],
            nv12_export: Nv12ExportEvidence {
                stride: 1920,
                offset: 0,
                uv_stride: 1920,
                uv_offset: 2_088_960,
                modifier: 0,
            },
            counters: FrameCounters {
                captured: 40,
                converted: 39,
                encoded: 38,
                sent: 37,
                skipped: 1,
                failures: 2,
            },
            transport: TransportEvidence {
                in_flight: 1,
                client_queue_depth: 2,
                ack_suspended: false,
            },
            config: BehaviourConfig {
                fps: 30,
                resolution: (1920, 1080),
                bitrate: 10_000_000,
                quality: 23,
                max_frames_in_flight: 3,
                capture_mode: "ext",
                codec: "avc420",
            },
        }
    }

    #[test]
    fn block_lists_every_required_fact_in_order() {
        let block = evidence(None).block("2026-10-29T14:20:00Z", 4242);

        let expected = [
            "=== hypr-rdp failure ===",
            "utc: 2026-10-29T14:20:00Z",
            "pid: 4242",
            "attempt: vaQuerySurfaceStatus poll on the VPP output surface",
            "va_status: none",
            "vpp_input: memory_types=0x30000001 import_path=DRM_PRIME (legacy)",
            "encoder_input: memory_types=query-failed import_path=not initialized",
            "capture_buffer[0]: fd=12 stride=7680 offset=0 modifier=0x0100000000000001 format=0x34325258",
            "nv12_export: stride=1920 offset=0 uv_stride=1920 uv_offset=2088960 modifier=0x0000000000000000",
            "counters: captured=40 converted=39 encoded=38 sent=37 skipped=1 failures=2",
            "transport: in_flight=1 client_queue_depth=2 ack_suspended=false",
            "config: fps=30 resolution=1920x1080 bitrate=10000000 quality=23 max_frames_in_flight=3 capture_mode=ext codec=avc420",
            "=== end ===",
        ];

        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines.len(), expected.len(), "{block}");
        for (line, expected) in lines.iter().zip(expected) {
            assert_eq!(*line, expected);
        }
    }

    #[test]
    fn block_keeps_the_raw_va_status_and_operation() {
        let block = evidence(Some(VaStatusEvidence {
            status: 4,
            operation: "vaSyncSurface (VPP output)",
            surface: 7,
        }))
        .block("2026-10-29T14:20:00Z", 1);

        assert!(
            block.contains("va_status: 4 (operation: vaSyncSurface (VPP output), surface: 7)"),
            "{block}"
        );
    }

    #[test]
    fn truncation_skips_the_partial_line_at_the_cut() {
        assert_eq!(start_of_next_line(b"half a block\nnext block\n"), 13);
        assert_eq!(start_of_next_line(b"no newline at all"), 0);
    }

    #[test]
    fn timestamp_renders_utc_without_a_date_dependency() {
        assert_eq!(utc_timestamp(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            utc_timestamp(UNIX_EPOCH + std::time::Duration::from_secs(951_827_696)),
            "2000-02-29T12:34:56Z"
        );
    }
}
