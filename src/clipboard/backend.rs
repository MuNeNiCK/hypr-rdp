use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::impl_as_any;
use ironrdp_pdu::IntoOwned;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;

use super::formats::{fix_bitfields_dib, utf16le_to_utf8, PendingWrite};
use super::wayland::clipboard_thread;

const MAX_CLIPBOARD_SIZE: usize = 100 * 1024 * 1024;

pub struct HyprCliprdrFactory {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl HyprCliprdrFactory {
    pub fn new() -> Self {
        Self { event_sender: None }
    }
}

impl ServerEventSender for HyprCliprdrFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.event_sender = Some(sender);
    }
}

impl CliprdrBackendFactory for HyprCliprdrFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        let clipboard_data = Arc::new(Mutex::new(None::<Vec<u8>>));
        let clipboard_image = Arc::new(Mutex::new(None::<Vec<u8>>));
        let pending_write = Arc::new(Mutex::new(None::<PendingWrite>));
        let suppress = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));

        Box::new(HyprCliprdrBackend {
            event_sender: self.event_sender.clone(),
            remote_formats: Vec::new(),
            watcher_thread: None,
            suppress_watcher: suppress,
            clipboard_data,
            clipboard_image,
            pending_write,
            running,
            last_requested_format: None,
        })
    }
}

impl CliprdrServerFactory for HyprCliprdrFactory {}

struct HyprCliprdrBackend {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    remote_formats: Vec<ClipboardFormat>,
    watcher_thread: Option<thread::JoinHandle<()>>,
    suppress_watcher: Arc<AtomicBool>,
    clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
    clipboard_image: Arc<Mutex<Option<Vec<u8>>>>, // CF_DIB bytes
    pending_write: Arc<Mutex<Option<PendingWrite>>>,
    running: Arc<AtomicBool>,
    last_requested_format: Option<ClipboardFormatId>,
}

impl_as_any!(HyprCliprdrBackend);

impl fmt::Debug for HyprCliprdrBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyprCliprdrBackend")
            .field("remote_formats", &self.remote_formats.len())
            .field("watching", &self.watcher_thread.is_some())
            .finish()
    }
}

impl Drop for HyprCliprdrBackend {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.watcher_thread.take() {
            let _ = handle.join();
        }
    }
}

impl CliprdrBackend for HyprCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        tracing::info!("Clipboard channel ready");
        self.start_clipboard_watcher();
    }

    fn on_request_format_list(&mut self) {
        let mut formats = Vec::new();

        let has_text = self
            .clipboard_data
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|d| !d.is_empty()))
            .unwrap_or(false);
        if has_text {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
        }

        let has_image = self
            .clipboard_image
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|d| !d.is_empty()))
            .unwrap_or(false);
        if has_image {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
        }

        if !formats.is_empty() {
            if let Some(ref sender) = self.event_sender {
                let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(
                    formats,
                )));
            }
        }
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        tracing::trace!(
            formats = available_formats.len(),
            "Clipboard: remote clipboard updated"
        );
        self.remote_formats = available_formats.to_vec();

        let has_unicode = available_formats
            .iter()
            .any(|f| f.id == ClipboardFormatId::CF_UNICODETEXT);
        let has_dibv5 = available_formats
            .iter()
            .any(|f| f.id == ClipboardFormatId::CF_DIBV5);
        let has_dib = available_formats
            .iter()
            .any(|f| f.id == ClipboardFormatId::CF_DIB);

        // Prefer text over image; prefer CF_DIBV5 over CF_DIB (better BITFIELDS support)
        let format = if has_unicode {
            Some(ClipboardFormatId::CF_UNICODETEXT)
        } else if has_dibv5 {
            Some(ClipboardFormatId::CF_DIBV5)
        } else if has_dib {
            Some(ClipboardFormatId::CF_DIB)
        } else {
            None
        };

        self.last_requested_format = format;

        if let Some(fmt) = format {
            if let Some(ref sender) = self.event_sender {
                let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(
                    fmt,
                )));
            }
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let response = if request.format == ClipboardFormatId::CF_UNICODETEXT {
            let data = self.clipboard_data.lock().ok().and_then(|g| g.clone());
            match data {
                Some(ref data) if !data.is_empty() => {
                    let text = String::from_utf8_lossy(data);
                    FormatDataResponse::new_unicode_string(&text).into_owned()
                }
                _ => FormatDataResponse::new_error().into_owned(),
            }
        } else if request.format == ClipboardFormatId::CF_DIB {
            let data = self.clipboard_image.lock().ok().and_then(|g| g.clone());
            match data {
                Some(dib_data) if !dib_data.is_empty() => {
                    FormatDataResponse::new_data(dib_data).into_owned()
                }
                _ => FormatDataResponse::new_error().into_owned(),
            }
        } else {
            FormatDataResponse::new_error().into_owned()
        };

        if let Some(ref sender) = self.event_sender {
            let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendFormatData(
                response,
            )));
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        self.handle_format_data_response(response, MAX_CLIPBOARD_SIZE);
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

impl HyprCliprdrBackend {
    fn handle_format_data_response(
        &mut self,
        response: FormatDataResponse<'_>,
        max_clipboard_size: usize,
    ) {
        let requested_format = self.last_requested_format.take();

        if response.is_error() {
            return;
        }

        let data = response.data();
        if data.is_empty() {
            return;
        }

        if data.len() > max_clipboard_size {
            tracing::warn!(
                size = data.len(),
                max = max_clipboard_size,
                "Clipboard data too large, ignoring"
            );
            return;
        }

        match requested_format {
            Some(ClipboardFormatId::CF_DIBV5) => {
                match ironrdp_cliprdr_format::bitmap::dibv5_to_png(data) {
                    Ok(png_data) => {
                        tracing::trace!(len = png_data.len(), "Clipboard: converted DIBV5 to PNG");
                        self.suppress_watcher.store(true, Ordering::SeqCst);
                        if let Ok(mut guard) = self.pending_write.lock() {
                            *guard = Some(PendingWrite::Image(png_data));
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Clipboard: failed to convert DIBV5 to PNG: {}", e);
                    }
                }
            }
            Some(ClipboardFormatId::CF_DIB) => {
                let png_result = ironrdp_cliprdr_format::bitmap::dib_to_png(data).or_else(|_| {
                    let fixed = fix_bitfields_dib(data).ok_or_else(|| {
                        ironrdp_cliprdr_format::bitmap::BitmapError::Unsupported(
                            "cannot fix BITFIELDS",
                        )
                    })?;
                    ironrdp_cliprdr_format::bitmap::dib_to_png(&fixed)
                });
                match png_result {
                    Ok(png_data) => {
                        tracing::trace!(len = png_data.len(), "Clipboard: converted DIB to PNG");
                        self.suppress_watcher.store(true, Ordering::SeqCst);
                        if let Ok(mut guard) = self.pending_write.lock() {
                            *guard = Some(PendingWrite::Image(png_data));
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Clipboard: failed to convert DIB to PNG: {}", e);
                    }
                }
            }
            Some(ClipboardFormatId::CF_UNICODETEXT) => {
                let utf8 = utf16le_to_utf8(data);
                if utf8.is_empty() {
                    return;
                }

                tracing::trace!(len = utf8.len(), "Clipboard: received text from RDP client");
                self.suppress_watcher.store(true, Ordering::SeqCst);
                if let Ok(mut guard) = self.pending_write.lock() {
                    *guard = Some(PendingWrite::Text(utf8.into_bytes()));
                }
            }
            Some(format) => {
                tracing::trace!(?format, "Clipboard: ignoring unrequested response format");
            }
            None => {
                tracing::trace!("Clipboard: ignoring format data response without pending request");
            }
        }
    }

    fn start_clipboard_watcher(&mut self) {
        let sender = match self.event_sender.clone() {
            Some(s) => s,
            None => return,
        };

        let suppress = Arc::clone(&self.suppress_watcher);
        let clipboard_data = Arc::clone(&self.clipboard_data);
        let clipboard_image = Arc::clone(&self.clipboard_image);
        let pending_write = Arc::clone(&self.pending_write);
        let running = Arc::clone(&self.running);

        let handle = thread::Builder::new()
            .name("clipboard-watcher".into())
            .spawn(move || {
                if let Err(e) = clipboard_thread(
                    sender,
                    suppress,
                    clipboard_data,
                    clipboard_image,
                    pending_write,
                    running,
                ) {
                    tracing::error!("Clipboard thread error: {:#}", e);
                }
            })
            .ok();

        if let Some(h) = handle {
            self.watcher_thread = Some(h);
            tracing::info!("Clipboard: watching via wlr-data-control-v1");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const ONE_BY_ONE_RGBA_PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, b'I', b'D', b'A', b'T', 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00,
        0x00, 0x00, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
    ];

    fn backend_with_events() -> (HyprCliprdrBackend, mpsc::UnboundedReceiver<ServerEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        (
            HyprCliprdrBackend {
                event_sender: Some(event_tx),
                remote_formats: Vec::new(),
                watcher_thread: None,
                suppress_watcher: Arc::new(AtomicBool::new(false)),
                clipboard_data: Arc::new(Mutex::new(None)),
                clipboard_image: Arc::new(Mutex::new(None)),
                pending_write: Arc::new(Mutex::new(None)),
                running: Arc::new(AtomicBool::new(true)),
                last_requested_format: None,
            },
            event_rx,
        )
    }

    fn recv_clipboard_event(
        event_rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
    ) -> ClipboardMessage {
        match event_rx.try_recv().expect("clipboard event queued") {
            ServerEvent::Clipboard(message) => message,
            other => panic!("unexpected server event: {other:?}"),
        }
    }

    fn decode_png(data: &[u8]) -> (u32, u32, png::ColorType, Vec<u8>) {
        let decoder = png::Decoder::new(Cursor::new(data));
        let mut reader = decoder.read_info().expect("PNG header decodes");
        let mut buffer = vec![0; reader.output_buffer_size().expect("PNG output buffer size")];
        let info = reader.next_frame(&mut buffer).expect("PNG frame decodes");

        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        buffer.truncate(info.buffer_size());
        (info.width, info.height, info.color_type, buffer)
    }

    fn assert_pending_image_pixel(
        backend: &HyprCliprdrBackend,
        color_type: png::ColorType,
        pixel: &[u8],
    ) {
        let pending = backend.pending_write.lock().unwrap();
        let PendingWrite::Image(data) = pending.as_ref().expect("pending write") else {
            panic!("expected image pending write");
        };
        let (width, height, actual_color_type, actual_pixel) = decode_png(data);

        assert_eq!((width, height), (1, 1));
        assert_eq!(actual_color_type, color_type);
        assert_eq!(actual_pixel, pixel);
    }

    fn bitfields_dib_from_png() -> Vec<u8> {
        let dib = ironrdp_cliprdr_format::bitmap::png_to_cf_dib(ONE_BY_ONE_RGBA_PNG)
            .expect("test PNG converts to DIB");
        let mut bitfields = Vec::with_capacity(dib.len() + 12);
        bitfields.extend_from_slice(&dib[..16]);
        bitfields.extend_from_slice(&3u32.to_le_bytes());
        bitfields.extend_from_slice(&dib[20..40]);
        bitfields.extend_from_slice(&0x00ff_0000u32.to_le_bytes());
        bitfields.extend_from_slice(&0x0000_ff00u32.to_le_bytes());
        bitfields.extend_from_slice(&0x0000_00ffu32.to_le_bytes());
        bitfields.extend_from_slice(&dib[40..]);
        bitfields
    }

    #[test]
    fn request_format_list_advertises_text_and_image_formats() {
        let (mut backend, mut event_rx) = backend_with_events();
        *backend.clipboard_data.lock().unwrap() = Some(b"hello".to_vec());
        *backend.clipboard_image.lock().unwrap() = Some(vec![1, 2, 3, 4]);

        backend.on_request_format_list();

        let ClipboardMessage::SendInitiateCopy(formats) = recv_clipboard_event(&mut event_rx)
        else {
            panic!("expected SendInitiateCopy");
        };
        let ids = formats.iter().map(|format| format.id).collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![ClipboardFormatId::CF_UNICODETEXT, ClipboardFormatId::CF_DIB]
        );
    }

    #[test]
    fn request_format_list_does_not_emit_empty_clipboard() {
        let (mut backend, mut event_rx) = backend_with_events();

        backend.on_request_format_list();

        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn remote_copy_prefers_unicode_then_dibv5_then_dib() {
        for (formats, expected) in [
            (
                vec![
                    ClipboardFormat::new(ClipboardFormatId::CF_DIB),
                    ClipboardFormat::new(ClipboardFormatId::CF_DIBV5),
                    ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
                ],
                ClipboardFormatId::CF_UNICODETEXT,
            ),
            (
                vec![
                    ClipboardFormat::new(ClipboardFormatId::CF_DIB),
                    ClipboardFormat::new(ClipboardFormatId::CF_DIBV5),
                ],
                ClipboardFormatId::CF_DIBV5,
            ),
            (
                vec![ClipboardFormat::new(ClipboardFormatId::CF_DIB)],
                ClipboardFormatId::CF_DIB,
            ),
        ] {
            let (mut backend, mut event_rx) = backend_with_events();

            backend.on_remote_copy(&formats);

            assert_eq!(backend.last_requested_format, Some(expected));
            let ClipboardMessage::SendInitiatePaste(format) = recv_clipboard_event(&mut event_rx)
            else {
                panic!("expected SendInitiatePaste");
            };
            assert_eq!(format, expected);
        }
    }

    #[test]
    fn remote_copy_ignores_unsupported_formats() {
        let (mut backend, mut event_rx) = backend_with_events();
        let formats = [ClipboardFormat::new(ClipboardFormatId::CF_TEXT)];

        backend.on_remote_copy(&formats);

        assert_eq!(backend.remote_formats, formats);
        assert_eq!(backend.last_requested_format, None);
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn remote_copy_clears_stale_requested_format_when_no_supported_format_exists() {
        let (mut backend, mut event_rx) = backend_with_events();

        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);
        let ClipboardMessage::SendInitiatePaste(format) = recv_clipboard_event(&mut event_rx)
        else {
            panic!("expected SendInitiatePaste");
        };
        assert_eq!(format, ClipboardFormatId::CF_UNICODETEXT);
        assert_eq!(
            backend.last_requested_format,
            Some(ClipboardFormatId::CF_UNICODETEXT)
        );

        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_TEXT)]);

        assert_eq!(backend.last_requested_format, None);
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn format_data_request_returns_unicode_text_response() {
        let (mut backend, mut event_rx) = backend_with_events();
        *backend.clipboard_data.lock().unwrap() = Some("hello".as_bytes().to_vec());

        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });

        let ClipboardMessage::SendFormatData(response) = recv_clipboard_event(&mut event_rx) else {
            panic!("expected SendFormatData");
        };
        assert!(!response.is_error());
        assert_eq!(
            response.data(),
            &[b'h', 0, b'e', 0, b'l', 0, b'l', 0, b'o', 0, 0, 0]
        );
    }

    #[test]
    fn format_data_response_writes_text_pending_for_wayland() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_UNICODETEXT);

        backend.on_format_data_response(FormatDataResponse::new_data(&[b'o', 0, b'k', 0, 0, 0]));

        assert!(backend.suppress_watcher.load(Ordering::SeqCst));
        let pending = backend.pending_write.lock().unwrap();
        match pending.as_ref().expect("pending write") {
            PendingWrite::Text(data) => assert_eq!(data, b"ok"),
            PendingWrite::Image(_) => panic!("expected text pending write"),
        }
    }

    #[test]
    fn format_data_response_without_pending_request_is_ignored() {
        let (mut backend, _event_rx) = backend_with_events();

        backend.on_format_data_response(FormatDataResponse::new_data(&[b'o', 0, b'k', 0, 0, 0]));

        assert!(!backend.suppress_watcher.load(Ordering::SeqCst));
        assert!(backend.pending_write.lock().unwrap().is_none());
    }

    #[test]
    fn late_format_data_response_after_unsupported_copy_is_ignored() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_UNICODETEXT);
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_TEXT)]);

        backend.on_format_data_response(FormatDataResponse::new_data(&[b'o', 0, b'k', 0, 0, 0]));

        assert_eq!(backend.last_requested_format, None);
        assert!(!backend.suppress_watcher.load(Ordering::SeqCst));
        assert!(backend.pending_write.lock().unwrap().is_none());
    }

    #[test]
    fn format_data_response_ignores_oversized_payload_without_mutating_pending_write() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_UNICODETEXT);
        *backend.pending_write.lock().unwrap() = Some(PendingWrite::Text(b"old".to_vec()));
        let oversized = [0, 0, 0, 0, 0];

        backend.handle_format_data_response(FormatDataResponse::new_data(&oversized), 4);

        assert!(!backend.suppress_watcher.load(Ordering::SeqCst));
        assert_eq!(backend.last_requested_format, None);
        let pending = backend.pending_write.lock().unwrap();
        match pending.as_ref().expect("existing pending write remains") {
            PendingWrite::Text(data) => assert_eq!(data, b"old"),
            PendingWrite::Image(_) => panic!("expected existing text pending write"),
        }
    }

    #[test]
    fn format_data_response_writes_dib_image_pending_for_wayland() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_DIB);
        let dib = ironrdp_cliprdr_format::bitmap::png_to_cf_dib(ONE_BY_ONE_RGBA_PNG)
            .expect("test PNG converts to DIB");

        backend.on_format_data_response(FormatDataResponse::new_data(&dib));

        assert!(backend.suppress_watcher.load(Ordering::SeqCst));
        assert_eq!(backend.last_requested_format, None);
        assert_pending_image_pixel(&backend, png::ColorType::Rgb, &[255, 0, 0]);
    }

    #[test]
    fn format_data_response_writes_dibv5_image_pending_for_wayland() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_DIBV5);
        let dibv5 = ironrdp_cliprdr_format::bitmap::png_to_cf_dibv5(ONE_BY_ONE_RGBA_PNG)
            .expect("test PNG converts to DIBV5");

        backend.on_format_data_response(FormatDataResponse::new_data(&dibv5));

        assert!(backend.suppress_watcher.load(Ordering::SeqCst));
        assert_eq!(backend.last_requested_format, None);
        assert_pending_image_pixel(&backend, png::ColorType::Rgba, &[255, 0, 0, 255]);
    }

    #[test]
    fn format_data_response_repairs_bitfields_dib_before_png_conversion() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_DIB);
        let dib = bitfields_dib_from_png();

        backend.on_format_data_response(FormatDataResponse::new_data(&dib));

        assert!(backend.suppress_watcher.load(Ordering::SeqCst));
        assert_eq!(backend.last_requested_format, None);
        assert_pending_image_pixel(&backend, png::ColorType::Rgb, &[255, 0, 0]);
    }

    #[test]
    fn format_data_response_ignores_corrupt_dib_without_pending_write() {
        let (mut backend, _event_rx) = backend_with_events();
        backend.last_requested_format = Some(ClipboardFormatId::CF_DIB);

        backend.on_format_data_response(FormatDataResponse::new_data(b"not a dib"));

        assert!(!backend.suppress_watcher.load(Ordering::SeqCst));
        assert_eq!(backend.last_requested_format, None);
        assert!(backend.pending_write.lock().unwrap().is_none());
    }
}
