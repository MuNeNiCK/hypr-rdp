use super::*;

pub(super) struct AppState {
    pub(super) tx: mpsc::Sender<DisplayUpdate>,
    pub(super) target_output_name: String,
    // Globals
    pub(super) shm: Option<wl_shm::WlShm>,
    pub(super) target_output: Option<wl_output::WlOutput>,
    pub(super) outputs: Vec<(u32, wl_output::WlOutput)>, // (name_id, output)
    pub(super) output_names: Vec<(u32, String)>,         // (wl_output id, name)
    pub(super) capture_manager:
        Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,
    pub(super) source_manager:
        Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    pub(super) screencopy_manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    #[cfg(feature = "vaapi")]
    pub(super) linux_dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    // Session state
    pub(super) session: Option<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1>,
    pub(super) buffer_width: u32,
    pub(super) buffer_height: u32,
    pub(super) wlr_stride: u32,
    pub(super) shm_format: Option<wl_shm::Format>,
    // DMA-BUF session state
    #[cfg(feature = "vaapi")]
    pub(super) dmabuf_device: Option<libc::dev_t>,
    #[cfg(feature = "vaapi")]
    pub(super) dmabuf_formats: Vec<(u32, Vec<u64>)>, // (drm_format, modifiers)
    // Frame state
    pub(super) frame_ready: bool,
    pub(super) frame_failed: bool,
    pub(super) damage_regions: Vec<(i32, i32, i32, i32)>,
    pub(super) stopped: bool,
    pub(super) stop_flag: Arc<std::sync::atomic::AtomicBool>,
}

impl AppState {
    pub(super) fn new(
        tx: mpsc::Sender<DisplayUpdate>,
        target_output_name: String,
        stop_flag: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            tx,
            target_output_name,
            shm: None,
            target_output: None,
            outputs: Vec::new(),
            output_names: Vec::new(),
            capture_manager: None,
            source_manager: None,
            screencopy_manager: None,
            #[cfg(feature = "vaapi")]
            linux_dmabuf: None,
            session: None,
            buffer_width: 0,
            buffer_height: 0,
            wlr_stride: 0,
            shm_format: None,
            #[cfg(feature = "vaapi")]
            dmabuf_device: None,
            #[cfg(feature = "vaapi")]
            dmabuf_formats: Vec::new(),
            frame_ready: false,
            frame_failed: false,
            damage_regions: Vec::new(),
            stopped: false,
            stop_flag,
        }
    }

    pub(super) fn should_stop(&self) -> bool {
        self.tx.is_closed()
            || self.stopped
            || self.stop_flag.load(std::sync::atomic::Ordering::Acquire)
    }
}

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
                    state.capture_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.source_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zwlr_screencopy_manager_v1" => {
                    state.screencopy_manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                #[cfg(feature = "vaapi")]
                "zwp_linux_dmabuf_v1" => {
                    state.linux_dmabuf = Some(registry.bind(name, version.min(4), qh, ()));
                }
                _ => {}
            }
        }
    }
}

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
            // Find which output this proxy belongs to
            let proxy_id = proxy.id().protocol_id();
            state.output_names.push((proxy_id, name.clone()));
            if name == state.target_output_name {
                // Find the matching output in our stored list
                for (_, output) in &state.outputs {
                    if output.id().protocol_id() == proxy_id {
                        state.target_output = Some(output.clone());
                        tracing::trace!(name = %name, "Matched target output");
                        break;
                    }
                }
            }
        }
    }
}

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
                state.buffer_width = width;
                state.buffer_height = height;
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat {
                format: WEnum::Value(fmt),
            } => match fmt {
                wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => {
                    state.shm_format = Some(fmt);
                }
                _ => {
                    if state.shm_format.is_none() {
                        state.shm_format = Some(fmt);
                    }
                }
            },
            ext_image_copy_capture_session_v1::Event::Done => {}
            ext_image_copy_capture_session_v1::Event::Stopped => {
                tracing::warn!("Session stopped");
                state.stopped = true;
            }
            #[cfg(feature = "vaapi")]
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                // device is a Vec<u8> containing a dev_t value
                if device.len() >= std::mem::size_of::<libc::dev_t>() {
                    let dev = libc::dev_t::from_ne_bytes(
                        device[..std::mem::size_of::<libc::dev_t>()]
                            .try_into()
                            .unwrap(),
                    );
                    tracing::trace!(dev, "Session: DMA-BUF device advertised");
                    state.dmabuf_device = Some(dev);
                }
            }
            #[cfg(feature = "vaapi")]
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                // modifiers is a Vec<u8> containing an array of u64 values
                let mut mods = Vec::new();
                let chunk_size = std::mem::size_of::<u64>();
                let mut i = 0;
                while i + chunk_size <= modifiers.len() {
                    let m = u64::from_ne_bytes(modifiers[i..i + chunk_size].try_into().unwrap());
                    mods.push(m);
                    i += chunk_size;
                }
                tracing::trace!(
                    format = format!("0x{:08x}", format),
                    num_modifiers = mods.len(),
                    "Session: DMA-BUF format advertised"
                );
                state.dmabuf_formats.push((format, mods));
            }
            _ => {}
        }
    }
}

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
            ext_image_copy_capture_frame_v1::Event::Failed { .. } => {
                state.frame_failed = true;
            }
            ext_image_copy_capture_frame_v1::Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                state.damage_regions.push((x, y, width, height));
            }
            _ => {}
        }
    }
}

delegate_noop!(AppState: ignore wl_shm::WlShm);
delegate_noop!(AppState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(AppState: ignore wl_buffer::WlBuffer);
delegate_noop!(AppState: ignore ext_image_capture_source_v1::ExtImageCaptureSourceV1);
delegate_noop!(AppState: ignore ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(AppState: ignore ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1);
delegate_noop!(AppState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
#[cfg(feature = "vaapi")]
delegate_noop!(AppState: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
#[cfg(feature = "vaapi")]
delegate_noop!(AppState: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                // Use the first suitable format
                if state.buffer_width == 0 {
                    state.buffer_width = width;
                    state.buffer_height = height;
                    state.wlr_stride = stride;
                    state.shm_format = Some(format);
                }
                // Prefer Xrgb8888
                if format == wl_shm::Format::Xrgb8888 || format == wl_shm::Format::Argb8888 {
                    state.buffer_width = width;
                    state.buffer_height = height;
                    state.wlr_stride = stride;
                    state.shm_format = Some(format);
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.frame_ready = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.frame_failed = true;
            }
            zwlr_screencopy_frame_v1::Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                state
                    .damage_regions
                    .push((x as i32, y as i32, width as i32, height as i32));
            }
            _ => {}
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_display::WlDisplay, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_display::WlDisplay,
        _: wayland_client::protocol::wl_display::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
