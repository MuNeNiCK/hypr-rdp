use std::num::{NonZeroU16, NonZeroUsize};
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::io::OwnedFd;
use std::process::Command;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use ironrdp_server::{BitmapUpdate, DisplayUpdate, PixelFormat};
use tokio::sync::mpsc;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1;
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1,
    ext_image_copy_capture_session_v1,
};

pub struct CaptureInfo {
    pub width: u32,
    pub height: u32,
    /// Name of the headless output created for this session
    pub output_name: String,
}

/// Create a headless output in Hyprland at the given resolution.
/// Returns the output name (e.g. "HEADLESS-1").
fn create_headless_output(width: u32, height: u32) -> Result<String> {
    // Create headless output
    let output = Command::new("hyprctl")
        .args(["output", "create", "headless"])
        .output()
        .context("failed to run hyprctl output create")?;
    if !output.status.success() {
        bail!("hyprctl output create failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Find the newly created headless output name
    let monitors_output = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .context("failed to run hyprctl monitors")?;
    let monitors: serde_json::Value = serde_json::from_slice(&monitors_output.stdout)
        .context("failed to parse hyprctl monitors output")?;

    let name = monitors
        .as_array()
        .context("expected monitors array")?
        .iter()
        .filter_map(|m| {
            let n = m["name"].as_str()?;
            if n.starts_with("HEADLESS-") {
                Some(n.to_string())
            } else {
                None
            }
        })
        .last()
        .context("no HEADLESS output found after creation")?;

    // Set resolution
    let mode = format!("{}x{}@60", width, height);
    let rule = format!("{},{},-9999x0,1", name, mode);
    let result = Command::new("hyprctl")
        .args(["keyword", "monitor", &rule])
        .output()
        .context("failed to set headless output resolution")?;
    if !result.status.success() {
        bail!("hyprctl keyword monitor failed: {}", String::from_utf8_lossy(&result.stderr));
    }

    tracing::info!(name = %name, width, height, "Created headless output");
    Ok(name)
}

/// Remove a headless output from Hyprland.
pub fn remove_headless_output(name: &str) {
    match Command::new("hyprctl")
        .args(["output", "remove", name])
        .output()
    {
        Ok(output) if output.status.success() => {
            tracing::info!(name, "Removed headless output");
        }
        Ok(output) => {
            tracing::warn!(name, stderr = %String::from_utf8_lossy(&output.stderr), "Failed to remove headless output");
        }
        Err(e) => {
            tracing::warn!(name, error = %e, "Failed to run hyprctl output remove");
        }
    }
}

/// Start screen capture on a background thread.
/// Creates a headless output at the target resolution and captures it.
pub async fn start_capture(tx: mpsc::Sender<DisplayUpdate>, target_resolution: (u32, u32)) -> Result<CaptureInfo> {
    let (info_tx, info_rx) = tokio::sync::oneshot::channel();

    std::thread::Builder::new()
        .name("wayland-capture".into())
        .spawn(move || {
            if let Err(e) = capture_thread(tx, info_tx, target_resolution) {
                tracing::error!("Capture thread error: {:#}", e);
            }
        })?;

    info_rx.await.context("capture thread failed to start")?
}

fn capture_thread(
    tx: mpsc::Sender<DisplayUpdate>,
    info_tx: tokio::sync::oneshot::Sender<Result<CaptureInfo>>,
    target_resolution: (u32, u32),
) -> Result<()> {
    let (target_w, target_h) = target_resolution;

    // Create headless output at the target resolution
    let output_name = match create_headless_output(target_w, target_h) {
        Ok(name) => name,
        Err(e) => {
            let _ = info_tx.send(Err(e));
            return Ok(());
        }
    };

    // Small delay to let Hyprland set up the output
    std::thread::sleep(std::time::Duration::from_millis(500));

    let conn = Connection::connect_to_env().context("failed to connect to Wayland display")?;
    let mut event_queue = conn.new_event_queue::<AppState>();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = AppState::new(tx, output_name.clone());

    // First roundtrip: collect globals
    event_queue
        .roundtrip(&mut state)
        .context("Wayland roundtrip failed")?;

    // Second roundtrip: get output names
    event_queue
        .roundtrip(&mut state)
        .context("Wayland roundtrip (2nd) failed")?;

    // Verify required globals are present
    let capture_mgr = state
        .capture_manager
        .as_ref()
        .context("ext_image_copy_capture_manager_v1 not available")?
        .clone();
    let source_mgr = state
        .source_manager
        .as_ref()
        .context("ext_output_image_capture_source_manager_v1 not available")?
        .clone();
    let output = state
        .target_output
        .as_ref()
        .context(format!("headless output '{}' not found in Wayland globals", state.target_output_name))?
        .clone();
    let shm = state
        .shm
        .as_ref()
        .context("wl_shm not available")?
        .clone();

    // Create capture source from the headless output
    let source = source_mgr.create_source(&output, &qh, ());

    // Create capture session (with cursors painted)
    let session = capture_mgr.create_session(
        &source,
        ext_image_copy_capture_manager_v1::Options::PaintCursors,
        &qh,
        (),
    );
    state.session = Some(session.clone());

    // Roundtrip to get buffer constraints
    event_queue
        .roundtrip(&mut state)
        .context("failed to get buffer constraints")?;
    event_queue
        .roundtrip(&mut state)
        .context("failed to get buffer constraints (2nd)")?;

    let width = state.buffer_width;
    let height = state.buffer_height;

    if width == 0 || height == 0 {
        let err = anyhow::anyhow!("invalid buffer dimensions: {}x{}", width, height);
        remove_headless_output(&output_name);
        let _ = info_tx.send(Err(err));
        return Ok(());
    }

    let shm_format = state.shm_format.unwrap_or(wl_shm::Format::Xrgb8888);
    let stride = width * 4;
    let buf_size = (stride * height) as usize;

    // Create shared memory buffer
    let shm_fd = create_shm_fd(buf_size)?;
    let pool = shm.create_pool(shm_fd.as_fd(), buf_size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        shm_format,
        &qh,
        (),
    );

    // Memory-map the buffer for reading
    let mmap = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            buf_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            shm_fd.as_fd().as_raw_fd(),
            0,
        )
    };
    if mmap == libc::MAP_FAILED {
        remove_headless_output(&output_name);
        bail!("mmap failed");
    }

    // Send capture info back — no scaling needed, headless output is at target resolution
    let _ = info_tx.send(Ok(CaptureInfo {
        width,
        height,
        output_name: output_name.clone(),
    }));

    let pixel_format = match shm_format {
        wl_shm::Format::Argb8888 => PixelFormat::BgrA32,
        wl_shm::Format::Xrgb8888 => PixelFormat::BgrX32,
        wl_shm::Format::Xbgr8888 => PixelFormat::RgbX32,
        wl_shm::Format::Abgr8888 => PixelFormat::RgbA32,
        _ => PixelFormat::BgrA32,
    };

    tracing::info!(
        width, height, ?shm_format,
        output = %output_name,
        "Starting capture loop on headless output"
    );

    // Frame rate limiting
    let target_interval = std::time::Duration::from_millis(33); // ~30fps
    let mut last_frame = std::time::Instant::now();

    // Capture loop
    loop {
        if state.tx.is_closed() {
            tracing::info!("Display update channel closed, stopping capture");
            break;
        }

        if state.stopped {
            tracing::warn!("Capture session stopped by compositor");
            break;
        }

        let frame = session.create_frame(&qh, ());
        state.frame_ready = false;
        state.frame_failed = false;
        state.damage_regions.clear();

        frame.attach_buffer(&buffer);
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();

        loop {
            event_queue
                .blocking_dispatch(&mut state)
                .context("Wayland dispatch failed")?;
            if state.frame_ready || state.frame_failed {
                break;
            }
        }

        frame.destroy();

        if state.frame_failed {
            tracing::debug!("Frame capture failed, retrying");
            continue;
        }

        // Read pixels directly — no scaling needed
        let data = unsafe { std::slice::from_raw_parts(mmap as *const u8, buf_size) };

        let update = DisplayUpdate::Bitmap(BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(width as u16).unwrap(),
            height: NonZeroU16::new(height as u16).unwrap(),
            format: pixel_format,
            data: Bytes::copy_from_slice(data),
            stride: NonZeroUsize::new(stride as usize).unwrap(),
        });

        if state.tx.blocking_send(update).is_err() {
            tracing::info!("Display update channel closed");
            break;
        }

        let elapsed = last_frame.elapsed();
        if elapsed < target_interval {
            std::thread::sleep(target_interval - elapsed);
        }
        last_frame = std::time::Instant::now();
    }

    // Cleanup
    unsafe {
        libc::munmap(mmap, buf_size);
    }
    remove_headless_output(&output_name);

    Ok(())
}

fn create_shm_fd(size: usize) -> Result<OwnedFd> {
    use std::ffi::CStr;
    let name = CStr::from_bytes_with_nul(b"hypr-rdp-shm\0").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create failed");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let ret = unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) };
    if ret < 0 {
        bail!("ftruncate failed");
    }
    Ok(fd)
}

// --- Wayland state ---

struct AppState {
    tx: mpsc::Sender<DisplayUpdate>,
    target_output_name: String,
    // Globals
    shm: Option<wl_shm::WlShm>,
    target_output: Option<wl_output::WlOutput>,
    outputs: Vec<(u32, wl_output::WlOutput)>, // (name_id, output)
    output_names: Vec<(u32, String)>,           // (wl_output id, name)
    capture_manager: Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,
    source_manager:
        Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    // Session state
    session: Option<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1>,
    buffer_width: u32,
    buffer_height: u32,
    shm_format: Option<wl_shm::Format>,
    // Frame state
    frame_ready: bool,
    frame_failed: bool,
    damage_regions: Vec<(i32, i32, i32, i32)>,
    stopped: bool,
}

impl AppState {
    fn new(tx: mpsc::Sender<DisplayUpdate>, target_output_name: String) -> Self {
        Self {
            tx,
            target_output_name,
            shm: None,
            target_output: None,
            outputs: Vec::new(),
            output_names: Vec::new(),
            capture_manager: None,
            source_manager: None,
            session: None,
            buffer_width: 0,
            buffer_height: 0,
            shm_format: None,
            frame_ready: false,
            frame_failed: false,
            damage_regions: Vec::new(),
            stopped: false,
        }
    }
}

// --- Registry dispatch ---

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_output" => {
                    let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    state.outputs.push((name, output));
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.capture_manager =
                        Some(registry.bind(name, version.min(1), qh, ()));
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.source_manager =
                        Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }
}

// --- Output dispatch (to get output name) ---

impl Dispatch<wl_output::WlOutput, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            tracing::debug!(name = %name, "Found output");
            // Find which output this proxy belongs to
            let proxy_id = proxy.id().protocol_id();
            state.output_names.push((proxy_id, name.clone()));
            if name == state.target_output_name {
                // Find the matching output in our stored list
                for (_, output) in &state.outputs {
                    if output.id().protocol_id() == proxy_id {
                        state.target_output = Some(output.clone());
                        tracing::info!(name = %name, "Matched target output");
                        break;
                    }
                }
            }
        }
    }
}

// --- Session dispatch ---

impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                tracing::debug!(width, height, "Buffer size");
                state.buffer_width = width;
                state.buffer_height = height;
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                if let WEnum::Value(fmt) = format {
                    tracing::debug!(?fmt, "SHM format");
                    match fmt {
                        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => {
                            state.shm_format = Some(fmt);
                        }
                        _ => {
                            if state.shm_format.is_none() {
                                state.shm_format = Some(fmt);
                            }
                        }
                    }
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                tracing::debug!("Session constraints done");
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                tracing::warn!("Session stopped");
                state.stopped = true;
            }
            _ => {}
        }
    }
}

// --- Frame dispatch ---

impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => {
                state.frame_ready = true;
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                tracing::debug!(?reason, "Frame failed");
                state.frame_failed = true;
            }
            ext_image_copy_capture_frame_v1::Event::Damage {
                x, y, width, height,
            } => {
                state.damage_regions.push((x, y, width, height));
            }
            _ => {}
        }
    }
}

// --- No-op dispatchers ---

delegate_noop!(AppState: ignore wl_shm::WlShm);
delegate_noop!(AppState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(AppState: ignore wl_buffer::WlBuffer);
delegate_noop!(AppState: ignore ext_image_capture_source_v1::ExtImageCaptureSourceV1);
delegate_noop!(AppState: ignore ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(AppState: ignore ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1);

impl Dispatch<wayland_client::protocol::wl_display::WlDisplay, ()> for AppState {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_display::WlDisplay, _: wayland_client::protocol::wl_display::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for AppState {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_callback::WlCallback, _: wayland_client::protocol::wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
