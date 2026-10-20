//! Client-to-server selection lifetime; mounting stays off the protocol executor.

#[cfg(feature = "client-to-server")]
mod filesystem;
#[cfg(feature = "client-to-server")]
mod mount;
#[cfg_attr(not(feature = "client-to-server"), allow(dead_code))]
mod transfer;
#[cfg_attr(not(feature = "client-to-server"), allow(dead_code))]
mod tree;

use super::formats::PendingWrite;
use ironrdp_cliprdr::pdu::{FileContentsResponse, FileDescriptor};
use ironrdp_server::ServerEvent;
#[cfg(feature = "client-to-server")]
use std::sync::Condvar;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use transfer::Transfer;

#[derive(Clone)]
pub(super) struct InboundHandle {
    transfer: Transfer,
    pending: Arc<Mutex<Option<PendingWrite>>>,
    #[cfg(feature = "client-to-server")]
    service: Arc<(Mutex<Service>, Condvar)>,
}

impl InboundHandle {
    /// Supersedes the advertised selection. Whatever was already mounted keeps
    /// serving its open handles until its lock is released.
    pub(super) fn retire(&self) {
        let _ = self.retire_selection();
    }

    fn retire_selection(&self) -> Option<u64> {
        let (generation, _) = self.transfer.retire_generation();
        self.drop_unpublished_files();
        #[cfg(feature = "client-to-server")]
        self.update_service(None, false);
        generation
    }

    /// IronRDP sent the `Unlock` for these clipboard-data locks, so the remote
    /// may drop their bytes: the selections they covered stop serving.
    pub(super) fn release_locks(&self, data_ids: &[u32]) {
        let current = self.transfer.generation();
        let released = self.transfer.release_locks(data_ids);
        if current.is_some_and(|generation| released.contains(&generation)) {
            self.drop_unpublished_files();
        }
        if released.is_empty() {
            return;
        }
        #[cfg(feature = "client-to-server")]
        self.update_service(None, false);
    }

    fn drop_unpublished_files(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            if matches!(*pending, Some(PendingWrite::Files { .. })) {
                *pending = None;
            }
        }
    }

    #[cfg(all(test, feature = "client-to-server"))]
    pub(super) fn read_first_file(
        &self,
        size: u32,
    ) -> tokio::task::JoinHandle<Result<Vec<u8>, ()>> {
        let generation = self.transfer.generation().unwrap();
        let inode = self
            .transfer
            .with_tree(generation, |tree| tree.roots()[0])
            .unwrap();
        let transfer = self.transfer.clone();
        self.transfer
            .runtime
            .spawn(async move { transfer.read(generation, inode, 0, size).await })
    }

    #[cfg(feature = "client-to-server")]
    fn update_service(&self, job: Option<Job>, close: bool) {
        let (state, changed) = &*self.service;
        if let Ok(mut state) = state.lock() {
            state.closed |= close;
            if job.is_some() {
                state.job = job;
            }
            state.dirty = true;
            changed.notify_one();
        }
    }
}

pub(super) struct InboundClipboard {
    handle: InboundHandle,
}

impl InboundClipboard {
    pub(super) fn available() -> bool {
        cfg!(feature = "client-to-server")
    }

    pub(super) fn new(
        sender: mpsc::UnboundedSender<ServerEvent>,
        pending: Arc<Mutex<Option<PendingWrite>>>,
        max_entries: usize,
        max_chunk: u32,
    ) -> Self {
        let handle = InboundHandle {
            transfer: Transfer::new(sender, max_entries, max_chunk),
            pending,
            #[cfg(feature = "client-to-server")]
            service: Arc::new((
                Mutex::new(Service {
                    closed: false,
                    dirty: false,
                    job: None,
                }),
                Condvar::new(),
            )),
        };
        #[cfg(feature = "client-to-server")]
        {
            let worker = handle.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("clipboard-inbound-mount".into())
                .spawn(move || serve(worker, mount::RemoteMount::create))
            {
                tracing::warn!(%error, "Clipboard: cannot start inbound mount service");
                handle.transfer.close();
            }
        }
        Self { handle }
    }

    pub(super) fn handle(&self) -> InboundHandle {
        self.handle.clone()
    }
    pub(super) fn enabled(&self) -> bool {
        Self::available() && self.handle.transfer.enabled()
    }
    pub(super) fn generation(&self) -> Option<u64> {
        self.handle.transfer.generation()
    }
    pub(super) fn begin_remote_copy(&self) -> Option<u64> {
        self.handle.retire_selection()
    }
    pub(super) fn set_capabilities(&self, stream: bool, huge: bool, locking: bool) {
        self.handle.drop_unpublished_files();
        self.handle.transfer.capabilities(stream, huge, locking);
        #[cfg(feature = "client-to-server")]
        self.handle.update_service(None, false);
    }
    pub(super) fn release_locks(&self, data_ids: &[u32]) {
        self.handle.release_locks(data_ids);
    }
    pub(super) fn on_response(&self, response: FileContentsResponse<'_>) {
        self.handle.transfer.on_response(response);
    }
    #[cfg(all(test, feature = "client-to-server"))]
    fn accept(&self, files: &[FileDescriptor], data_id: Option<u32>) {
        if let Some(generation) = self.generation() {
            self.accept_for(generation, files, data_id);
        }
    }

    pub(super) fn accept_for(
        &self,
        generation: u64,
        files: &[FileDescriptor],
        data_id: Option<u32>,
    ) {
        #[cfg(feature = "client-to-server")]
        if let Some((generation, roots, _)) =
            self.handle.transfer.accept_for(generation, files, data_id)
        {
            self.handle
                .update_service(Some(Job { generation, roots }), false);
        }
        #[cfg(not(feature = "client-to-server"))]
        let _ = (generation, files, data_id);
    }
}

impl Drop for InboundClipboard {
    fn drop(&mut self) {
        self.handle.transfer.close();
        self.handle.drop_unpublished_files();
        #[cfg(feature = "client-to-server")]
        self.handle.update_service(None, true);
    }
}

#[cfg(feature = "client-to-server")]
struct Job {
    generation: u64,
    roots: Vec<String>,
}
#[cfg(feature = "client-to-server")]
struct Service {
    closed: bool,
    dirty: bool,
    job: Option<Job>,
}

#[cfg(feature = "client-to-server")]
trait Mounted {
    fn path(&self) -> &std::path::Path;
}
#[cfg(feature = "client-to-server")]
impl Mounted for mount::RemoteMount {
    fn path(&self) -> &std::path::Path {
        self.path()
    }
}

#[cfg(feature = "client-to-server")]
fn serve<M: Mounted>(
    handle: InboundHandle,
    mut create: impl FnMut(filesystem::RemoteFilesystem) -> Option<M>,
) {
    // One mount per selection that is still serving: the live one, plus every
    // retired one whose clipboard-data lock the remote still honours.
    let mut mounted: std::collections::HashMap<u64, M> = std::collections::HashMap::new();
    loop {
        let (state, changed) = &*handle.service;
        let job = {
            let Ok(state) = state.lock() else { break };
            let Ok(mut state) = changed.wait_while(state, |state| !state.dirty && !state.closed)
            else {
                break;
            };
            if state.closed {
                break;
            }
            state.dirty = false;
            state.job.take()
        };
        // Reconcile against the transfer owner instead of accumulating release
        // notifications while a filesystem operation is blocked.
        mounted.retain(|generation, _| handle.transfer.with_tree(*generation, |_| ()).is_some());
        let Some(job) = job else { continue };
        if job.roots.is_empty()
            || handle
                .transfer
                .with_live_tree(job.generation, |_| ())
                .is_none()
        {
            continue;
        }
        let filesystem = filesystem::RemoteFilesystem::new(handle.transfer.clone(), job.generation);
        let Some(mount) = create(filesystem) else {
            continue;
        };
        // Creation may block while a new copy, local owner change, or close arrives.
        // Only the still-live generation can publish its newly created path.
        let published = handle
            .transfer
            .with_live_tree(job.generation, |_| {
                let Some(payload) = uri_payload(mount.path(), &job.roots) else {
                    return false;
                };
                let Ok(mut pending) = handle.pending.lock() else {
                    return false;
                };
                *pending = Some(payload);
                true
            })
            .unwrap_or(false);
        if published {
            mounted.insert(job.generation, mount);
        }
    }
    // M's destructor runs here, never on a protocol callback or Wayland watcher.
}

#[cfg(feature = "client-to-server")]
fn uri_payload(mount: &std::path::Path, roots: &[String]) -> Option<PendingWrite> {
    let uris: Vec<_> = roots
        .iter()
        .map(|name| {
            url::Url::from_file_path(mount.join(name))
                .ok()
                .map(String::from)
        })
        .collect::<Option<_>>()?;
    if uris.is_empty() {
        return None;
    }
    Some(PendingWrite::Files {
        uri_list: uris
            .iter()
            .map(|uri| format!("{uri}\r\n"))
            .collect::<String>()
            .into_bytes(),
        gnome_copied_files: format!("copy\n{}", uris.join("\n")).into_bytes(),
    })
}

#[cfg(all(test, feature = "client-to-server"))]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    struct FakeMount {
        dropped: std::sync::mpsc::Sender<()>,
    }
    impl Mounted for FakeMount {
        fn path(&self) -> &Path {
            Path::new("/fake/selection")
        }
    }
    impl Drop for FakeMount {
        fn drop(&mut self) {
            self.dropped.send(()).unwrap();
        }
    }

    fn unstarted() -> InboundClipboard {
        let (sender, _) = mpsc::unbounded_channel();
        InboundClipboard {
            handle: InboundHandle {
                transfer: Transfer::new(sender, 100, 1024),
                pending: Arc::new(Mutex::new(None)),
                service: Arc::new((
                    Mutex::new(Service {
                        closed: false,
                        dirty: false,
                        job: None,
                    }),
                    Condvar::new(),
                )),
            },
        }
    }

    #[tokio::test]
    async fn inbound_mount_publication_respects_generation_and_close() {
        for close in [false, true] {
            let clipboard = unstarted();
            clipboard.set_capabilities(true, true, true);
            let pending = Arc::clone(&clipboard.handle.pending);
            let handle = clipboard.handle();
            let (started, started_rx) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            let (dropped, dropped_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                serve(handle, |_| {
                    started.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    Some(FakeMount {
                        dropped: dropped.clone(),
                    })
                })
            });
            clipboard.accept(&[FileDescriptor::new("old").with_file_size(1)], None);
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let clipboard = if close {
                drop(clipboard);
                None
            } else {
                clipboard.handle.retire();
                Some(clipboard)
            };
            release.send(()).unwrap();
            dropped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(
                pending.lock().unwrap().is_none(),
                "stale mount must never reach Wayland"
            );
            drop(clipboard);
            worker.join().unwrap();
        }
    }

    #[tokio::test]
    async fn inbound_mount_failure_keeps_other_clipboard_content() {
        let clipboard = unstarted();
        clipboard.set_capabilities(true, true, true);
        let pending = Arc::clone(&clipboard.handle.pending);
        *pending.lock().unwrap() = Some(PendingWrite::Text(b"usable".to_vec()));
        let handle = clipboard.handle();
        let (attempted, attempts) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve::<FakeMount>(handle, |_| {
                attempted.send(()).unwrap();
                None
            })
        });
        clipboard.accept(&[FileDescriptor::new("file").with_file_size(1)], None);
        attempts.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(clipboard);
        worker.join().unwrap();
        assert!(
            matches!(pending.lock().unwrap().as_ref(), Some(PendingWrite::Text(bytes)) if bytes == b"usable")
        );
    }

    #[tokio::test]
    async fn inbound_mount_success_publishes_exact_selection() {
        let clipboard = unstarted();
        clipboard.set_capabilities(true, true, true);
        let handle = clipboard.handle();
        let (dropped, drops) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            serve(handle, |_| {
                Some(FakeMount {
                    dropped: dropped.clone(),
                })
            })
        });
        clipboard.accept(&[FileDescriptor::new("selected").with_file_size(1)], None);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if clipboard.handle.pending.lock().unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            clipboard
                .handle
                .pending
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .data_for_mime(super::super::formats::FILE_URI_LIST_MIME)
                .unwrap(),
            b"file:///fake/selection/selected\r\n"
        );
        drop(clipboard);
        drops.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn inbound_uri_payload_lists_only_roots_and_encodes_names() {
        let payload = uri_payload(Path::new("/mount"), &["a b#é".into(), "folder".into()]).unwrap();
        assert_eq!(
            payload
                .data_for_mime(super::super::formats::FILE_URI_LIST_MIME)
                .unwrap(),
            b"file:///mount/a%20b%23%C3%A9\r\nfile:///mount/folder\r\n"
        );
        assert_eq!(
            payload
                .data_for_mime(super::super::formats::GNOME_COPIED_FILES_MIME)
                .unwrap(),
            b"copy\nfile:///mount/a%20b%23%C3%A9\nfile:///mount/folder"
        );
    }

    #[tokio::test]
    async fn inbound_retained_mounts_release_only_the_matching_generation_and_close_all() {
        struct TaggedMount {
            path: std::path::PathBuf,
            dropped: std::sync::mpsc::Sender<std::path::PathBuf>,
        }
        impl Mounted for TaggedMount {
            fn path(&self) -> &Path {
                &self.path
            }
        }
        impl Drop for TaggedMount {
            fn drop(&mut self) {
                self.dropped.send(self.path.clone()).unwrap();
            }
        }
        let clipboard = unstarted();
        clipboard.set_capabilities(true, true, true);
        let handle = clipboard.handle();
        let (dropped, drops) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut serial = 0;
            serve(handle, |_| {
                serial += 1;
                Some(TaggedMount {
                    path: format!("/fake/{serial}").into(),
                    dropped: dropped.clone(),
                })
            });
        });
        for id in [77, 88, 99] {
            clipboard.accept(&[FileDescriptor::new("file").with_file_size(1)], Some(id));
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if clipboard.handle.pending.lock().unwrap().take().is_some() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        }
        assert!(
            drops.try_recv().is_err(),
            "replacement must retain all three mounts"
        );
        clipboard.release_locks(&[77]);
        assert_eq!(
            drops.recv_timeout(Duration::from_secs(2)).unwrap(),
            Path::new("/fake/1")
        );
        assert!(
            drops.try_recv().is_err(),
            "A's unlock must not unmount B or C"
        );
        drop(clipboard);
        worker.join().unwrap();
        let mut remaining = vec![
            drops.recv_timeout(Duration::from_secs(2)).unwrap(),
            drops.recv_timeout(Duration::from_secs(2)).unwrap(),
        ];
        remaining.sort();
        assert_eq!(remaining, [Path::new("/fake/2"), Path::new("/fake/3")]);
    }

    /// Runs only in a FUSE-capable validation environment. The oracle copies bytes
    /// through production lookup/getattr/read, including the unknown-size path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a FUSE-capable guest or CI runner with fusermount3"]
    async fn inbound_real_mount_copy_bytes() {
        use ironrdp_cliprdr::backend::ClipboardMessage;
        use ironrdp_cliprdr::pdu::{ClipboardFileAttributes, FileContentsFlags};
        use std::os::unix::fs::DirBuilderExt;

        let runtime =
            std::env::temp_dir().join(format!("hypr-rdp-byte-copy-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&runtime)
            .unwrap();
        let previous_runtime = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        let (sender, mut events) = mpsc::unbounded_channel();
        let pending = Arc::new(Mutex::new(None));
        let clipboard = Arc::new(InboundClipboard::new(sender, Arc::clone(&pending), 100, 3));
        clipboard.set_capabilities(true, true, true);
        clipboard.accept(
            &[
                FileDescriptor::new("folder").with_attributes(ClipboardFileAttributes::DIRECTORY),
                FileDescriptor::new("known")
                    .with_relative_path("folder")
                    .with_file_size(11),
                FileDescriptor::new("unknown").with_relative_path("folder"),
                FileDescriptor::new("empty")
                    .with_relative_path("folder")
                    .with_attributes(ClipboardFileAttributes::DIRECTORY),
            ],
            Some(77),
        );
        let (read_started, started) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let responder = Arc::clone(&clipboard);
        let responder = tokio::spawn(async move {
            let mut barrier = Some((read_started, resumed));
            while let Some(ServerEvent::Clipboard(ClipboardMessage::SendFileContentsRequest(req))) =
                events.recv().await
            {
                if req.flags == FileContentsFlags::RANGE {
                    if let Some((started, resumed)) = barrier.take() {
                        started.send(()).unwrap();
                        resumed.await.unwrap();
                    }
                }
                let bytes = match (req.data_id, req.index) {
                    (Some(88), 0) => b"replacement bytes".as_slice(),
                    (Some(77), 1) => b"hello world".as_slice(),
                    (Some(77), 2) => b"unknown bytes".as_slice(),
                    _ => panic!("bad descriptor index"),
                };
                let response = if req.flags == FileContentsFlags::SIZE {
                    FileContentsResponse::new_size_response(req.stream_id, bytes.len() as u64)
                } else {
                    let start = req.position as usize;
                    // Exercise short positive ranges as well as max-chunk progression.
                    let end = (start + req.requested_size.min(2) as usize).min(bytes.len());
                    FileContentsResponse::new_data_response(
                        req.stream_id,
                        bytes[start..end].to_vec(),
                    )
                };
                responder.on_response(response);
            }
        });
        let source = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(PendingWrite::Files { uri_list, .. }) = pending.lock().unwrap().take() {
                    let uri = std::str::from_utf8(&uri_list).unwrap().trim();
                    break url::Url::parse(uri).unwrap().to_file_path().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("mount must publish file URIs");
        let mount_path = source.parent().unwrap().to_owned();
        let destination = runtime.join("copied");
        std::fs::create_dir(&destination).unwrap();
        let copied = tokio::task::spawn_blocking(move || {
            fn copy_dir(source: &Path, destination: &Path) {
                for entry in std::fs::read_dir(source).unwrap() {
                    let entry = entry.unwrap();
                    let dest = destination.join(entry.file_name());
                    if entry.file_type().unwrap().is_dir() {
                        std::fs::create_dir(&dest).unwrap();
                        copy_dir(&entry.path(), &dest);
                    } else {
                        std::fs::copy(entry.path(), dest).unwrap();
                    }
                }
            }
            copy_dir(&source, &destination);
            assert_eq!(
                std::fs::read(destination.join("known")).unwrap(),
                b"hello world"
            );
            assert_eq!(
                std::fs::read(destination.join("unknown")).unwrap(),
                b"unknown bytes"
            );
            assert!(destination.join("empty").is_dir());
        });
        tokio::time::timeout(Duration::from_secs(10), started)
            .await
            .unwrap()
            .unwrap();
        // Replace the clipboard while the kernel copy is waiting for A's bytes.
        clipboard.accept(
            &[FileDescriptor::new("replacement").with_file_size(17)],
            Some(88),
        );
        let replacement = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(PendingWrite::Files { uri_list, .. }) = pending.lock().unwrap().take() {
                    break url::Url::parse(std::str::from_utf8(&uri_list).unwrap().trim())
                        .unwrap()
                        .to_file_path()
                        .unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let replacement_mount = replacement.parent().unwrap().to_owned();
        assert_ne!(mount_path, replacement_mount);
        resume.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), copied)
            .await
            .unwrap()
            .unwrap();
        let replacement_copy = replacement.clone();
        let destination = runtime.join("replacement-copy");
        let copied = tokio::task::spawn_blocking(move || {
            std::fs::copy(replacement_copy, &destination).unwrap();
            assert_eq!(std::fs::read(destination).unwrap(), b"replacement bytes");
        });
        tokio::time::timeout(Duration::from_secs(10), copied)
            .await
            .unwrap()
            .unwrap();
        clipboard.release_locks(&[77]);
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime
                .read_dir()
                .unwrap()
                .any(|entry| entry.unwrap().path() == mount_path)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("only A's mount must be released");
        assert!(
            replacement_mount.is_dir(),
            "B's mount must survive A's unlock"
        );
        responder.abort();
        let _ = responder.await;
        drop(clipboard);
        let mount_name = replacement_mount.file_name().unwrap();
        let mut claim_name = mount_name.to_owned();
        claim_name.push(".lock");
        tokio::time::timeout(Duration::from_secs(5), async {
            // An invalidated FUSE root can return EIO before it is unmounted;
            // exists() would report false and race cleanup against the mount service.
            while std::fs::read_dir(&runtime).unwrap().any(|entry| {
                let name = entry.unwrap().file_name();
                name == mount_name || name == claim_name
            }) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("mount service must remove its directory on close");
        match previous_runtime {
            Some(path) => std::env::set_var("XDG_RUNTIME_DIR", path),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
        std::fs::remove_dir_all(runtime).unwrap();
    }
}
