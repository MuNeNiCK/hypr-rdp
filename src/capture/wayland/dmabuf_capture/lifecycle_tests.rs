//! Exercise production acquisition and teardown against API resource ledgers.
//! The driver shim runs in an isolated subprocess; no GPU/compositor is needed.
use super::*;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::thread;
use std::time::Duration;
use wayland_protocols::ext::image_capture_source::v1::server::{
    ext_image_capture_source_v1 as source,
    ext_output_image_capture_source_manager_v1 as source_manager,
};
use wayland_protocols::ext::image_copy_capture::v1::server::{
    ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_manager_v1 as manager,
    ext_image_copy_capture_session_v1 as session,
};
use wayland_protocols::wp::linux_dmabuf::zv1::server::{
    zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_v1 as dmabuf,
};
use wayland_server::protocol::{wl_buffer as server_buffer, wl_output};
use wayland_server::Dispatch;
use wayland_server::{Client, DataInit, Display, DisplayHandle, GlobalDispatch, New};

#[derive(Default)]
struct Ledger {
    params: usize,
    buffers: usize,
    frames: usize,
    captures: usize,
}

struct Peer {
    ledger: Arc<Mutex<Ledger>>,
    capture_stop: Arc<AtomicBool>,
    mode: String,
}

macro_rules! global {
    ($ty:ty) => {
        impl GlobalDispatch<$ty, ()> for Peer {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$ty>,
                _: &(),
                init: &mut DataInit<'_, Self>,
            ) {
                init.init(resource, ());
            }
        }
    };
}
global!(dmabuf::ZwpLinuxDmabufV1);
global!(manager::ExtImageCopyCaptureManagerV1);
global!(source_manager::ExtOutputImageCaptureSourceManagerV1);
global!(wl_output::WlOutput);

impl Dispatch<dmabuf::ZwpLinuxDmabufV1, ()> for Peer {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &dmabuf::ZwpLinuxDmabufV1,
        request: dmabuf::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            dmabuf::Request::CreateParams { params_id } => {
                state.ledger.lock().unwrap().params += 1;
                init.init(params_id, Mutex::new(Vec::<OwnedFd>::new()));
            }
            dmabuf::Request::Destroy => {}
            other => panic!("unexpected dmabuf request {other:?}"),
        }
    }
}
impl Dispatch<params::ZwpLinuxBufferParamsV1, Mutex<Vec<OwnedFd>>> for Peer {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &params::ZwpLinuxBufferParamsV1,
        request: params::Request,
        fds: &Mutex<Vec<OwnedFd>>,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            params::Request::Add { fd, plane_idx, .. } => {
                assert_eq!(plane_idx, 0);
                fds.lock().unwrap().push(fd);
            }
            params::Request::CreateImmed { buffer_id, .. } => {
                state.ledger.lock().unwrap().buffers += 1;
                init.init(buffer_id, std::mem::take(&mut *fds.lock().unwrap()));
            }
            params::Request::Destroy => {
                state.ledger.lock().unwrap().params -= 1;
            }
            other => panic!("unexpected params request {other:?}"),
        }
    }
}
impl Dispatch<server_buffer::WlBuffer, Vec<OwnedFd>> for Peer {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &server_buffer::WlBuffer,
        request: server_buffer::Request,
        _: &Vec<OwnedFd>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        assert!(matches!(request, server_buffer::Request::Destroy));
        let mut ledger = state.ledger.lock().unwrap();
        assert_eq!(
            ledger.frames, 0,
            "buffer destroyed before pending capture frame"
        );
        ledger.buffers -= 1;
    }
}
impl Dispatch<manager::ExtImageCopyCaptureManagerV1, ()> for Peer {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &manager::ExtImageCopyCaptureManagerV1,
        request: manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            manager::Request::CreateSession { session, .. } => {
                let session = init.init(session, ());
                session.buffer_size(16, 16);
                session.done();
            }
            manager::Request::Destroy => {}
            other => panic!("unexpected manager request {other:?}"),
        }
    }
}
impl Dispatch<source_manager::ExtOutputImageCaptureSourceManagerV1, ()> for Peer {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &source_manager::ExtOutputImageCaptureSourceManagerV1,
        request: source_manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let source_manager::Request::CreateSource { source, .. } = request {
            init.init(source, ());
        }
    }
}
impl Dispatch<session::ExtImageCopyCaptureSessionV1, ()> for Peer {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &session::ExtImageCopyCaptureSessionV1,
        request: session::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            session::Request::CreateFrame { frame } => {
                let mut ledger = state.ledger.lock().unwrap();
                assert_eq!(ledger.frames, 0, "new frame before old frame destruction");
                ledger.frames += 1;
                init.init(frame, ());
            }
            session::Request::Destroy => {}
            other => panic!("unexpected session request {other:?}"),
        }
    }
}
impl Dispatch<frame::ExtImageCopyCaptureFrameV1, ()> for Peer {
    fn request(
        state: &mut Self,
        _: &Client,
        frame: &frame::ExtImageCopyCaptureFrameV1,
        request: frame::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            frame::Request::Destroy => {
                state.ledger.lock().unwrap().frames -= 1;
            }
            frame::Request::Capture => {
                state.ledger.lock().unwrap().captures += 1;
                if state.mode == "stop" {
                    state.capture_stop.store(true, Ordering::Release);
                } else {
                    frame.damage(0, 0, 16, 16);
                    frame.ready();
                }
            }
            frame::Request::AttachBuffer { .. } | frame::Request::DamageBuffer { .. } => {}
            other => panic!("unexpected frame request {other:?}"),
        }
    }
}
macro_rules! destroy_only {
    ($ty:ty) => {
        impl Dispatch<$ty, ()> for Peer {
            fn request(
                _: &mut Self,
                _: &Client,
                _: &$ty,
                _: <$ty as wayland_server::Resource>::Request,
                _: &(),
                _: &DisplayHandle,
                _: &mut DataInit<'_, Self>,
            ) {
            }
        }
    };
}
destroy_only!(wl_output::WlOutput);
destroy_only!(source::ExtImageCaptureSourceV1);

struct Harness {
    conn: Connection,
    queue: wayland_client::EventQueue<AppState>,
    state: AppState,
    ledger: Arc<Mutex<Ledger>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    _rx: tokio::sync::mpsc::Receiver<DisplayUpdate>,
}
impl Harness {
    fn new(mode: &str) -> Self {
        let (client, server) = UnixStream::pair().unwrap();
        let ledger = Arc::new(Mutex::new(Ledger::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let capture_stop = Arc::new(AtomicBool::new(false));
        let mut peer = Peer {
            ledger: ledger.clone(),
            capture_stop: capture_stop.clone(),
            mode: mode.into(),
        };
        let peer_stop = stop.clone();
        let thread = thread::spawn(move || {
            let mut display = Display::<Peer>::new().unwrap();
            let mut handle = display.handle();
            handle.create_global::<Peer, dmabuf::ZwpLinuxDmabufV1, _>(4, ());
            handle.create_global::<Peer, manager::ExtImageCopyCaptureManagerV1, _>(1, ());
            handle.create_global::<Peer, source_manager::ExtOutputImageCaptureSourceManagerV1, _>(
                1,
                (),
            );
            handle.create_global::<Peer, wl_output::WlOutput, _>(1, ());
            handle.insert_client(server, Arc::new(())).unwrap();
            while !peer_stop.load(Ordering::Acquire) {
                display.dispatch_clients(&mut peer).unwrap();
                display.flush_clients().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });
        let conn = Connection::from_socket(client).unwrap();
        let mut queue = conn.new_event_queue();
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let mut state = AppState::new(tx, "test-output".into(), capture_stop);
        conn.display().get_registry(&queue.handle(), ());
        queue.roundtrip(&mut state).unwrap();
        queue.roundtrip(&mut state).unwrap();
        Self {
            conn,
            queue,
            state,
            ledger,
            stop,
            thread: Some(thread),
            _rx: rx,
        }
    }
    fn sync(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }
    fn setup(&self) -> Result<DmaBufCaptureContext> {
        setup_dmabuf_on_device(
            std::path::Path::new("/dev/null"),
            self.state.linux_dmabuf.as_ref().unwrap(),
            &self.queue.handle(),
            16,
            16,
            crate::capture::dmabuf::DRM_FORMAT_XRGB8888,
            &[],
        )
    }
    fn assert_empty(&mut self) {
        self.sync();
        let ledger = self.ledger.lock().unwrap();
        assert_eq!(ledger.params, 0, "params retained");
        assert_eq!(ledger.buffers, 0, "DMA-BUF buffers retained");
        assert_eq!(ledger.frames, 0, "capture frame retained");
    }
    fn run_loop(&mut self, context: &DmaBufCaptureContext, mode: &str) {
        let qh = self.queue.handle();
        let source = self.state.source_manager.as_ref().unwrap().create_source(
            &self.state.outputs[0].1,
            &qh,
            (),
        );
        let session = self.state.capture_manager.as_ref().unwrap().create_session(&source, wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::Options::empty(), &qh, ());
        self.sync();
        let layout = Arc::new(SharedOutputLayout::new());
        layout
            .update_snapshot_for_test("test-output", 16, 16, 16, 16, 0, 0, (16, 16))
            .unwrap();
        let gfx = crate::egfx::test_support::negotiated_no_avc_session(16, 16);
        if mode == "receiver-closed" {
            self._rx.close();
        }
        let initial_resize = (mode == "resize").then_some(DesktopSize {
            width: 16,
            height: 16,
        });
        let result = capture_loop_ext_dmabuf(
            &self.conn,
            &mut self.queue,
            &mut self.state,
            &qh,
            &session,
            16,
            16,
            context,
            Some(gfx.shared),
            1000,
            24,
            H264RateControl::Vbr,
            20,
            H264BackendPolicy::Auto,
            initial_resize,
            layout,
        );
        if self.state.stop_flag.load(Ordering::Acquire)
            || mode == "resize"
            || mode == "receiver-closed"
        {
            result.unwrap();
        } else {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("EGFX activation timed out"));
        }
        if mode == "resize" {
            assert!(matches!(
                self._rx.try_recv().unwrap(),
                DisplayUpdate::Resize(DesktopSize {
                    width: 16,
                    height: 16
                })
            ));
        }
        session.destroy();
        source.destroy();
        self.sync();
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

fn run_case(case: &str) {
    if case == "vpp-order" || case == "vpp-multi" {
        let before = fd_count();
        if case == "vpp-multi" {
            std::env::set_var("HYPR_RDP_DMABUF_FAIL", "multi-object");
        }
        for _ in 0..5 {
            let mut converter =
                crate::egfx::VppConverter::new(std::path::Path::new("/dev/null"), 16, 16).unwrap();
            let result = converter.export_nv12_output();
            assert_eq!(
                result.is_ok(),
                case == "vpp-order",
                "unsupported multi-object export accepted"
            );
        }
        assert_eq!(fd_count(), before, "exported FD retained");
    } else {
        let mut h = Harness::new(case);
        let before = fd_count();
        if !matches!(
            case,
            "fallback" | "stop" | "setup" | "resize" | "receiver-closed"
        ) {
            std::env::set_var("HYPR_RDP_DMABUF_FAIL", case);
        }
        let result = h.setup();
        if matches!(
            case,
            "fallback" | "stop" | "setup" | "resize" | "receiver-closed"
        ) {
            let context = result.unwrap();
            if case != "setup" {
                h.run_loop(&context, case);
            }
            drop(context);
        } else {
            assert!(
                result.is_err(),
                "failure stage {case} unexpectedly succeeded"
            );
        }
        h.assert_empty();
        assert_eq!(fd_count(), before, "FD retained at {case}");
        std::env::remove_var("HYPR_RDP_DMABUF_FAIL");
        // The same Wayland connection must permit a clean retry after failure.
        for _ in 0..3 {
            drop(h.setup().unwrap());
            h.assert_empty();
        }
        assert_eq!(fd_count(), before, "retry retained FD");
    }
    let log = std::fs::read_to_string(std::env::var_os("HYPR_RDP_DMABUF_LOG").unwrap()).unwrap();
    assert!(
        !log.contains("ORDER_ERROR"),
        "invalid VA teardown order:\n{log}"
    );
    assert!(
        log.lines()
            .filter(|line| line.starts_with("device-"))
            .all(|line| line == "device- bos=0 displays=0"),
        "GBM parent released before children:\n{log}"
    );
}

#[test]
fn dmabuf_lifecycle_releases_resources() {
    if let Ok(case) = std::env::var("HYPR_RDP_DMABUF_CASE") {
        thread::spawn(|| {
            thread::sleep(Duration::from_secs(10));
            std::process::exit(124);
        });
        return run_case(&case);
    }
    let path =
        std::env::temp_dir().join(format!("hypr-rdp-dmabuf-lifecycle-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(path.clone());
    let library = path.join("driver.so");
    let compiled = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/capture/wayland/dmabuf_capture/lifecycle_driver.c"
        ))
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let mut failures = Vec::new();
    for case in [
        "vpp-order",
        "vpp-multi",
        "setup",
        "fallback",
        "stop",
        "resize",
        "receiver-closed",
        "device",
        "bo-1",
        "bo-2",
        "fd-1",
        "fd-2",
        "display",
        "initialize",
        "config",
        "surface",
        "context",
        "import-1",
        "import-2",
        "export",
        "multi-object",
        "empty-layers",
        "empty-objects",
        "invalid-fd",
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "capture::wayland::dmabuf_capture::lifecycle_tests::dmabuf_lifecycle_releases_resources", "--nocapture", "--test-threads=1"])
            .env("LD_PRELOAD", &library).env("HYPR_RDP_DMABUF_CASE", case)
            .env("HYPR_RDP_DMABUF_LOG", path.join(format!("{case}.log")))
            .output().unwrap();
        if !output.status.success() {
            failures.push(format!(
                "{case}:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
