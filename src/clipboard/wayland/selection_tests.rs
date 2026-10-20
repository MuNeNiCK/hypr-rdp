//! A private Wayland protocol peer for clipboard offer ingress tests.
//! Uses generated server objects and real receive fds, without a compositor.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc as commands;
use std::thread::{self, JoinHandle};

use wayland_protocols_wlr::data_control::v1::server::{
    zwlr_data_control_device_v1 as device, zwlr_data_control_manager_v1 as manager,
    zwlr_data_control_offer_v1 as offer, zwlr_data_control_source_v1 as source,
};
use wayland_server::protocol::wl_seat;
use wayland_server::Dispatch as ServerDispatch;
use wayland_server::{Client, DataInit, Display, DisplayHandle, GlobalDispatch, New, Resource};

use super::*;

// None keeps the receive fd open without delivering a response.
type Representations = Vec<(String, Option<Vec<u8>>)>;

struct TextOfferPeer {
    device: Option<device::ZwlrDataControlDeviceV1>,
    completed: commands::Sender<()>,
    requested: Arc<Mutex<Vec<String>>>,
    stalled: Vec<OwnedFd>,
}

impl GlobalDispatch<wl_seat::WlSeat, ()> for TextOfferPeer {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_seat::WlSeat>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let seat = init.init(resource, ());
        seat.capabilities(wl_seat::Capability::empty());
    }
}

impl ServerDispatch<wl_seat::WlSeat, ()> for TextOfferPeer {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_seat::WlSeat,
        _: wl_seat::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<manager::ZwlrDataControlManagerV1, ()> for TextOfferPeer {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<manager::ZwlrDataControlManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl ServerDispatch<manager::ZwlrDataControlManagerV1, ()> for TextOfferPeer {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &manager::ZwlrDataControlManagerV1,
        request: manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            manager::Request::GetDataDevice { id, .. } => {
                let device = init.init(id, ());
                device.selection(None);
                state.device = Some(device);
            }
            manager::Request::CreateDataSource { id } => {
                init.init(id, ());
            }
            manager::Request::Destroy => {}
            other => panic!("unexpected manager request: {other:?}"),
        }
    }
}

impl ServerDispatch<device::ZwlrDataControlDeviceV1, ()> for TextOfferPeer {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &device::ZwlrDataControlDeviceV1,
        request: device::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        assert!(
            matches!(request, device::Request::Destroy),
            "unexpected device request: {request:?}"
        );
    }
}

impl ServerDispatch<source::ZwlrDataControlSourceV1, ()> for TextOfferPeer {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &source::ZwlrDataControlSourceV1,
        _: source::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl ServerDispatch<offer::ZwlrDataControlOfferV1, Representations> for TextOfferPeer {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &offer::ZwlrDataControlOfferV1,
        request: offer::Request,
        data: &Representations,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            offer::Request::Receive { mime_type, fd } => {
                state.requested.lock().unwrap().push(mime_type.clone());
                let bytes = &data
                    .iter()
                    .find(|(mime, _)| *mime == mime_type)
                    .expect("requested MIME was offered")
                    .1;
                let Some(bytes) = bytes else {
                    state.stalled.push(fd);
                    return;
                };
                // Fixtures fit inside a pipe; a missing read must not block the peer thread.
                assert!(bytes.len() < 4096);
                File::from(fd).write_all(bytes).unwrap();
            }
            offer::Request::Destroy => {
                state.stalled.clear();
                let _ = state.completed.send(());
            }
            other => panic!("unexpected offer request: {other:?}"),
        }
    }
}

pub(in crate::clipboard) struct TextOfferHarness {
    connection: Connection,
    queue: EventQueue<ClipState>,
    state: ClipState,
    offers: commands::Sender<Representations>,
    completed: commands::Receiver<()>,
    requested: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    peer: Option<JoinHandle<()>>,
}

impl TextOfferHarness {
    pub(in crate::clipboard) fn new(shared: ClipboardShared) -> Self {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let (offers, incoming) = commands::channel();
        let (finished, completed) = commands::channel();
        let requested = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let peer_requested = Arc::clone(&requested);
        let peer_stop = Arc::clone(&stop);
        let peer = thread::spawn(move || {
            let mut display = Display::<TextOfferPeer>::new().unwrap();
            let mut handle = display.handle();
            handle.create_global::<TextOfferPeer, wl_seat::WlSeat, _>(1, ());
            handle.create_global::<TextOfferPeer, manager::ZwlrDataControlManagerV1, _>(1, ());
            handle.insert_client(server_socket, Arc::new(())).unwrap();
            let mut state = TextOfferPeer {
                device: None,
                completed: finished,
                requested: peer_requested,
                stalled: Vec::new(),
            };
            let mut pending = VecDeque::new();
            while !peer_stop.load(Ordering::Relaxed) {
                display.dispatch_clients(&mut state).unwrap();
                pending.extend(incoming.try_iter());
                if let Some(device) = &state.device {
                    for representations in pending.drain(..) {
                        let client = device.client().unwrap();
                        let offer = client
                            .create_resource::<offer::ZwlrDataControlOfferV1, _, TextOfferPeer>(
                                &handle,
                                1,
                                representations,
                            )
                            .unwrap();
                        device.data_offer(&offer);
                        for (mime, _) in offer.data::<Representations>().unwrap() {
                            offer.offer(mime.clone());
                        }
                        device.selection(Some(&offer));
                    }
                }
                display.flush_clients().unwrap();
                thread::sleep(Duration::from_millis(2));
            }
        });

        let connection = Connection::from_socket(client_socket).unwrap();
        let queue = connection.new_event_queue::<ClipState>();
        let mut harness = Self {
            connection,
            queue,
            state: ClipState::new(shared),
            offers,
            completed,
            requested,
            stop,
            peer: Some(peer),
        };
        harness
            .connection
            .display()
            .get_registry(&harness.queue.handle(), ());
        assert!(dispatch_until_globals_ready(
            &harness.connection,
            &mut harness.queue,
            &mut harness.state,
            harness.connection.as_fd().as_raw_fd(),
            Instant::now() + Duration::from_secs(3),
        )
        .unwrap());
        let device = harness.state.manager.as_ref().unwrap().get_data_device(
            harness.state.seat.as_ref().unwrap(),
            &harness.queue.handle(),
            (),
        );
        harness.state.device = Some(device);
        harness.connection.flush().unwrap();
        harness
    }

    /// Wait for offer.destroy, which follows the actual production Selection handler.
    pub(in crate::clipboard) fn publish(
        &mut self,
        representations: &[(&str, &[u8])],
    ) -> Vec<String> {
        self.publish_owned(
            representations
                .iter()
                .map(|(mime, bytes)| (mime.to_string(), Some(bytes.to_vec())))
                .collect(),
        )
    }

    pub(in crate::clipboard) fn publish_stalled(&mut self, mime: &str) -> Vec<String> {
        self.publish_owned(vec![(mime.to_owned(), None)])
    }

    fn publish_owned(&mut self, representations: Representations) -> Vec<String> {
        self.requested.lock().unwrap().clear();
        self.offers.send(representations).unwrap();
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            self.queue.dispatch_pending(&mut self.state).unwrap();
            self.connection.flush().unwrap();
            if self.completed.try_recv().is_ok() {
                return self.requested.lock().unwrap().clone();
            }
            assert!(
                Instant::now() < deadline,
                "Wayland offer dispatch did not complete"
            );
            if let Some(guard) = self.queue.prepare_read() {
                if poll_wayland_fd(
                    self.connection.as_fd().as_raw_fd(),
                    Duration::from_millis(10),
                )
                .unwrap()
                {
                    guard.read().unwrap();
                }
            }
        }
    }
}

impl Drop for TextOfferHarness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(peer) = self.peer.take() {
            let result = peer.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}
