use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use super::keyboard::{create_keymap_fd, generate_xkb_keymap, KeyboardStateTracker};
use super::layout::SharedOutputLayout;
use super::virtual_keyboard::{ZwpVirtualKeyboardManagerV1, ZwpVirtualKeyboardV1};

/// Shared state for sending input commands to the Wayland thread
pub(super) struct InputState {
    conn: Connection,
    #[allow(dead_code)]
    event_queue: EventQueue<WlState>,
    #[allow(dead_code)]
    wl_state: WlState,
    pub(super) vk: ZwpVirtualKeyboardV1,
    pub(super) vp: ZwlrVirtualPointerV1,
    pub(super) keyboard_state: KeyboardStateTracker,
    pub(super) output_layout: Arc<SharedOutputLayout>,
    epoch: Instant,
}

impl InputState {
    /// Monotonically increasing timestamp in milliseconds.
    pub(super) fn timestamp(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    /// Flush outgoing Wayland requests to the compositor.
    /// Dispatches pending events first (non-blocking) to prevent socket buffer
    /// backpressure, then flushes outgoing requests.
    pub(super) fn flush(&mut self) {
        // Dispatch any pending events from compositor (non-blocking).
        // Without this, unread events can accumulate in the socket buffer
        // and cause the compositor to stop reading our requests.
        if let Err(e) = self.event_queue.dispatch_pending(&mut self.wl_state) {
            tracing::trace!("Wayland dispatch_pending failed: {}", e);
        }

        if let Err(e) = self.conn.flush() {
            tracing::warn!("Wayland flush failed: {}", e);
        }
    }
}

pub struct HyprInputHandler {
    pub(super) state: Arc<Mutex<InputState>>,
}

impl HyprInputHandler {
    pub fn new(
        rdp_width: u16,
        rdp_height: u16,
        output_layout: Arc<SharedOutputLayout>,
    ) -> Result<Self> {
        let layout = output_layout
            .snapshot()
            .context("output layout not initialized")?;

        let conn = Connection::connect_to_env().context("failed to connect to Wayland display")?;
        let mut event_queue = conn.new_event_queue::<WlState>();
        let qh = event_queue.handle();

        let display = conn.display();
        let _registry = display.get_registry(&qh, ());

        let mut wl_state = WlState::default();

        // Collect globals
        event_queue
            .roundtrip(&mut wl_state)
            .context("Wayland roundtrip failed")?;

        // Second roundtrip to receive wl_output name events
        event_queue
            .roundtrip(&mut wl_state)
            .context("Wayland roundtrip (output names) failed")?;

        let seat = wl_state.seat.clone().context("wl_seat not found")?;
        let vk_mgr = wl_state
            .vk_manager
            .clone()
            .context("zwp_virtual_keyboard_manager_v1 not found")?;
        let vp_mgr = wl_state
            .vp_manager
            .clone()
            .context("zwlr_virtual_pointer_manager_v1 not found")?;

        // Create virtual keyboard
        let vk = vk_mgr.create_virtual_keyboard(&seat, &qh, ());

        // Create virtual pointer bound to the target output (enables correct
        // monitor focus for compositor keybindings like Super+N)
        let target_output = wl_state
            .outputs
            .iter()
            .find(|(_, name)| name.as_deref() == Some(&layout.output_name))
            .map(|(o, _)| o)
            .context(format!("wl_output '{}' not found", layout.output_name))?;

        let vp =
            vp_mgr.create_virtual_pointer_with_output(Some(&seat), Some(target_output), &qh, ());

        // Release all wl_output proxies — they were only needed to find the
        // target output for create_virtual_pointer_with_output. Keeping them
        // alive would require dispatching their events; without that, the
        // compositor's send buffer fills up and blocks the event loop.
        for (output, _) in wl_state.outputs.drain(..) {
            output.release();
        }

        let (keymap_data, keymap_source) = load_keymap(&mut event_queue, &mut wl_state, &seat, &qh)
            .or_else(|err| {
                tracing::warn!(
                    "Failed to get compositor keymap, falling back to default: {:#}",
                    err
                );
                generate_xkb_keymap().map(|data| (data, "fallback"))
            })?;
        let keyboard_state = KeyboardStateTracker::new(&keymap_data)?;
        let keymap_fd = create_keymap_fd(&keymap_data)?;
        vk.keymap(1, keymap_fd.as_fd(), keymap_data.len() as u32); // 1 = XKB_V1
        keyboard_state.send_modifiers(&vk);

        // Flush to send all pending requests
        conn.flush()
            .context("Wayland flush after input setup failed")?;

        tracing::info!(
            rdp_width, rdp_height,
            layout_extent_w = layout.layout_extent_w,
            layout_extent_h = layout.layout_extent_h,
            output_offset_x = layout.output_offset_x,
            output_offset_y = layout.output_offset_y,
            output = %layout.output_name,
            keymap_source,
            "Input handler initialized (virtual keyboard + pointer)"
        );

        let state = Arc::new(Mutex::new(InputState {
            conn,
            event_queue,
            wl_state,
            vk,
            vp,
            keyboard_state,
            output_layout,
            epoch: Instant::now(),
        }));

        Ok(Self { state })
    }
}

fn load_keymap(
    event_queue: &mut EventQueue<WlState>,
    wl_state: &mut WlState,
    seat: &wl_seat::WlSeat,
    qh: &QueueHandle<WlState>,
) -> Result<(Vec<u8>, &'static str)> {
    if wl_state.seat_has_keyboard {
        if wl_state.keyboard.is_none() {
            wl_state.keyboard = Some(seat.get_keyboard(qh, ()));
        }

        event_queue
            .roundtrip(wl_state)
            .context("Wayland roundtrip for keyboard keymap failed")?;

        if let Some(keymap_data) = wl_state.keymap.take() {
            tracing::info!(len = keymap_data.len(), "Loaded compositor keyboard keymap");
            return Ok((keymap_data, "compositor"));
        }
    } else {
        tracing::warn!("Wayland seat has no keyboard capability, using fallback keymap");
    }

    let fallback = generate_xkb_keymap()?;
    tracing::info!(len = fallback.len(), "Generated fallback keyboard keymap");
    Ok((fallback, "fallback"))
}

fn read_keymap(fd: OwnedFd, size: u32) -> Result<Vec<u8>> {
    let size = usize::try_from(size).context("keyboard keymap too large")?;
    if size == 0 {
        bail!("keyboard keymap is empty");
    }

    let mut file = File::from(fd);
    let mut data = vec![0u8; size];
    file.read_exact(&mut data)
        .context("failed to read Wayland keyboard keymap")?;
    Ok(data)
}

#[derive(Default)]
struct WlState {
    seat: Option<wl_seat::WlSeat>,
    seat_has_keyboard: bool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keymap: Option<Vec<u8>>,
    vk_manager: Option<ZwpVirtualKeyboardManagerV1>,
    vp_manager: Option<ZwlrVirtualPointerManagerV1>,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for WlState {
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
                "wl_seat" => {
                    if state.seat.is_none() {
                        state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                    }
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.vk_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.vp_manager = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "wl_output" => {
                    let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    state.outputs.push((output, None));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            state.seat_has_keyboard = capabilities.contains(wl_seat::Capability::Keyboard);
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap {
            format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
            fd,
            size,
        } = event
        {
            match read_keymap(fd, size) {
                Ok(keymap) => {
                    state.keymap = Some(keymap);
                }
                Err(err) => {
                    tracing::warn!("Failed to read compositor keymap: {:#}", err);
                }
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for WlState {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(entry) = state.outputs.iter_mut().find(|(o, _)| o == proxy) {
                entry.1 = Some(name);
            }
        }
    }
}

delegate_noop!(WlState: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(WlState: ignore ZwpVirtualKeyboardV1);
delegate_noop!(WlState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(WlState: ignore ZwlrVirtualPointerV1);

impl Dispatch<wayland_client::protocol::wl_display::WlDisplay, ()> for WlState {
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
impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for WlState {
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
