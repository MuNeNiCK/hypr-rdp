//! Application-owned read admission and correlation, using IronRDP's fetch/PDU APIs.
//!
//! A selection survives being replaced. The clipboard-data lock we hold over the
//! remote's file list is what keeps its bytes available, so replacing the
//! clipboard *retires* the old selection instead of destroying it: its mount
//! stays up and its reads keep working, and only the lock's release -- reported
//! by IronRDP through `on_outgoing_locks_cleared` -- ends it.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::chunked_fetch::{ChunkedFetch, ChunkedFetchProgress};
use ironrdp_cliprdr::pdu::{
    FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor,
};
use ironrdp_server::ServerEvent;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

use super::tree::{RemoteNodeKind, RemoteTree};

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PENDING: usize = 64;
const MAX_BUFFERED: usize = 16 * 1024 * 1024;
const MAX_READ: u32 = 8 * 1024 * 1024;

/// How many superseded selections keep serving at once.
///
/// IronRDP's lock lifetime is the normal clock: a retired selection ends when
/// its `Unlock` goes out. This is the backstop for the paths that lock cannot
/// cover -- a local Wayland owner change expires nothing on the remote -- and
/// it bounds the mounts, not the memory, which stays under [`MAX_BUFFERED`]
/// across every selection at once.
const MAX_RETIRING: usize = 4;

#[derive(Clone)]
pub(super) struct Transfer {
    state: Arc<Mutex<State>>,
    sender: mpsc::UnboundedSender<ServerEvent>,
    pub(super) runtime: Handle,
    max_entries: usize,
    max_chunk: u32,
    timeout: Duration,
}

/// One announced file list, with the lock that keeps the remote's copy alive.
struct Selection {
    tree: RemoteTree,
    data_id: Option<u32>,
}

struct State {
    generation: u64,
    closed: bool,
    stream: bool,
    huge: bool,
    locking: bool,
    current: Selection,
    /// Superseded selections still serving open handles, by their generation.
    retiring: BTreeMap<u64, Selection>,
    /// Hands out inode ranges. An inode is never reused for a different file,
    /// across retired selections as well as successive ones.
    next_base: u64,
    next_stream: Option<u32>,
    /// Every outstanding request, tagged with the selection that issued it, so
    /// the budgets below stay global however many selections are alive.
    pending: HashMap<u32, Pending>,
}

enum Operation {
    Size { inode: u64 },
    Range { base: u64, fetch: ChunkedFetch },
}

enum Value {
    Size(u64),
    Bytes(Vec<u8>),
}
struct Pending {
    generation: u64,
    operation: Operation,
    answer: oneshot::Sender<Result<Value, ()>>,
    reserved: usize,
    _cancel_timeout: oneshot::Sender<()>,
}

impl State {
    /// The selection the Wayland clipboard currently advertises.
    fn live(&self, generation: u64) -> bool {
        !self.closed && self.stream && self.generation == generation
    }

    /// A selection that still answers filesystem requests: the live one, or one
    /// retired but not yet released.
    fn servable(&self, generation: u64) -> bool {
        !self.closed
            && self.stream
            && (self.generation == generation || self.retiring.contains_key(&generation))
    }

    fn selection(&self, generation: u64) -> Option<&Selection> {
        if !self.closed && self.stream {
            if self.generation == generation {
                return Some(&self.current);
            }
            return self.retiring.get(&generation);
        }
        None
    }

    fn selection_mut(&mut self, generation: u64) -> Option<&mut Selection> {
        if !self.closed && self.stream {
            if self.generation == generation {
                return Some(&mut self.current);
            }
            return self.retiring.get_mut(&generation);
        }
        None
    }

    /// Drops the answers of one selection, so every waiting read fails at once.
    fn fail_pending(&mut self, generation: u64) {
        self.pending
            .retain(|_, pending| pending.generation != generation);
    }

    fn take_current(&mut self) -> Selection {
        let base = self.current.tree.next_base();
        self.next_base = self.next_base.max(base);
        core::mem::replace(
            &mut self.current,
            Selection {
                tree: RemoteTree::empty(),
                data_id: None,
            },
        )
    }

    fn bump(&mut self) {
        match self.generation.checked_add(1) {
            Some(next) => self.generation = next,
            None => self.closed = true,
        }
    }

    /// Supersedes the live selection without ending it, and reports every
    /// generation whose mount must now come down.
    fn retire_current(&mut self) -> Vec<u64> {
        let superseded = self.generation;
        let selection = self.take_current();
        let mut released = Vec::new();

        if selection.tree.roots().is_empty() || selection.data_id.is_none() {
            // Nothing was ever mounted for it -- or, with no clipboard-data lock,
            // nothing keeps the client's copy alive to serve from. A client that
            // does not negotiate CAN_LOCK_CLIPDATA (FreeRDP 3.30.0 has the flag
            // commented out) therefore gets exactly the pre-locking behaviour,
            // rather than a mount that lists files it can no longer fetch.
            self.fail_pending(superseded);
            released.push(superseded);
        } else {
            self.retiring.insert(superseded, selection);
            while self.retiring.len() > MAX_RETIRING {
                let Some(oldest) = self.retiring.keys().next().copied() else {
                    break;
                };
                self.retiring.remove(&oldest);
                self.fail_pending(oldest);
                released.push(oldest);
            }
        }

        self.bump();
        released
    }

    /// Ends every selection, live and retired.
    fn shutdown(&mut self) -> Vec<u64> {
        let mut released: Vec<u64> = self.retiring.keys().copied().collect();
        released.push(self.generation);
        self.retiring.clear();
        self.pending.clear(); // Closing answers wakes every waiting filesystem request.
        self.take_current();
        self.bump();
        released
    }
}

impl Transfer {
    pub(super) fn new(
        sender: mpsc::UnboundedSender<ServerEvent>,
        max_entries: usize,
        max_chunk: u32,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                generation: 0,
                closed: false,
                stream: false,
                huge: false,
                locking: false,
                current: Selection {
                    tree: RemoteTree::empty(),
                    data_id: None,
                },
                retiring: BTreeMap::new(),
                next_base: 0,
                next_stream: Some(1),
                pending: HashMap::new(),
            })),
            sender,
            runtime: Handle::current(),
            max_entries,
            max_chunk: max_chunk.max(1),
            timeout: READ_TIMEOUT,
        }
    }

    pub(super) fn capabilities(&self, stream: bool, huge: bool, locking: bool) -> Vec<u64> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        let released = if state.stream != stream || state.huge != huge || state.locking != locking {
            state.shutdown()
        } else {
            Vec::new()
        };
        state.stream = stream;
        state.huge = huge;
        state.locking = locking;
        released
    }

    pub(super) fn enabled(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| state.stream && !state.closed)
    }

    pub(super) fn generation(&self) -> Option<u64> {
        let state = self.state.lock().ok()?;
        state.live(state.generation).then_some(state.generation)
    }

    #[cfg(test)]
    fn invalidate(&self) {
        let _ = self.retire_generation();
    }

    /// Supersedes the live selection. Retired selections keep serving.
    pub(super) fn retire_generation(&self) -> (Option<u64>, Vec<u64>) {
        let Ok(mut state) = self.state.lock() else {
            return (None, Vec::new());
        };
        let released = state.retire_current();
        let generation = state.live(state.generation).then_some(state.generation);
        (generation, released)
    }

    /// Releases the retired selections covered by the given clipboard-data
    /// locks. IronRDP reports these once it has sent their `Unlock`, at which
    /// point the remote is free to drop the bytes we were reading.
    pub(super) fn release_locks(&self, data_ids: &[u32]) -> Vec<u64> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        let mut released: Vec<u64> = state
            .retiring
            .iter()
            .filter(|(_, selection)| {
                selection
                    .data_id
                    .is_some_and(|data_id| data_ids.contains(&data_id))
            })
            .map(|(generation, _)| *generation)
            .collect();
        if state
            .current
            .data_id
            .is_some_and(|id| data_ids.contains(&id))
        {
            let generation = state.generation;
            state.take_current();
            state.fail_pending(generation);
            state.bump();
            released.push(generation);
        }
        for generation in &released {
            state.retiring.remove(generation);
            state.fail_pending(*generation);
        }
        released
    }

    pub(super) fn close(&self) -> Vec<u64> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        let released = state.shutdown();
        state.closed = true;
        released
    }

    #[cfg(test)]
    pub(super) fn accept(
        &self,
        files: &[FileDescriptor],
        data_id: Option<u32>,
    ) -> Option<(u64, Vec<String>)> {
        self.accept_for(self.generation()?, files, data_id)
            .map(|(generation, roots, _)| (generation, roots))
    }

    pub(super) fn accept_for(
        &self,
        expected: u64,
        files: &[FileDescriptor],
        data_id: Option<u32>,
    ) -> Option<(u64, Vec<String>, Vec<u64>)> {
        let mut state = self.state.lock().ok()?;
        if !state.live(expected) {
            return None;
        }
        let released = state.retire_current();
        if !state.live(state.generation) {
            return None;
        }
        let base = state.next_base;
        let tree = RemoteTree::build(files, self.max_entries, base);
        state.next_base = tree.next_base();
        let data_id = data_id.filter(|_| state.locking);
        state.current = Selection { tree, data_id };
        let roots = state
            .current
            .tree
            .roots()
            .iter()
            .filter_map(|inode| state.current.tree.node(*inode).map(|n| n.name.clone()))
            .collect();
        Some((state.generation, roots, released))
    }

    /// The closure and retirement are serialized, including publication to Wayland.
    pub(super) fn with_tree<T>(
        &self,
        generation: u64,
        f: impl FnOnce(&RemoteTree) -> T,
    ) -> Option<T> {
        let state = self.state.lock().ok()?;
        state.selection(generation).map(|s| f(&s.tree))
    }

    /// Like [`Self::with_tree`], but only for the selection Wayland advertises.
    pub(super) fn with_live_tree<T>(
        &self,
        generation: u64,
        f: impl FnOnce(&RemoteTree) -> T,
    ) -> Option<T> {
        let state = self.state.lock().ok()?;
        state.live(generation).then(|| f(&state.current.tree))
    }

    pub(super) async fn size(&self, generation: u64, inode: u64) -> Result<u64, ()> {
        let receiver = {
            let mut state = self.state.lock().map_err(|_| ())?;
            if !state.servable(generation) {
                return Err(());
            }
            let huge = state.huge;
            let selection = state.selection(generation).ok_or(())?;
            let node = selection.tree.node(inode).ok_or(())?;
            if let Some(size) = node.size {
                return if !huge && size > u64::from(u32::MAX) {
                    Err(())
                } else {
                    Ok(size)
                };
            }
            let RemoteNodeKind::File { index } = node.kind else {
                return Err(());
            };
            let data_id = selection.data_id;
            let id = Self::allocate(&mut state, 8)?;
            let request = FileContentsRequest {
                stream_id: id,
                index,
                flags: FileContentsFlags::SIZE,
                position: 0,
                requested_size: 8,
                data_id,
            };
            self.submit(
                &mut state,
                generation,
                request,
                Operation::Size { inode },
                8,
            )?
        };
        match receiver.await.map_err(|_| ())?? {
            Value::Size(size) => Ok(size),
            _ => Err(()),
        }
    }

    pub(super) async fn read(
        &self,
        generation: u64,
        inode: u64,
        position: u64,
        size: u32,
    ) -> Result<Vec<u8>, ()> {
        self.size(generation, inode).await?;
        let receiver = {
            let mut state = self.state.lock().map_err(|_| ())?;
            if !state.servable(generation) || size > MAX_READ {
                return Err(());
            }
            let selection = state.selection(generation).ok_or(())?;
            let node = selection.tree.node(inode).ok_or(())?;
            let RemoteNodeKind::File { index } = node.kind else {
                return Err(());
            };
            let amount = node
                .size
                .ok_or(())?
                .saturating_sub(position)
                .min(u64::from(size));
            if amount == 0 {
                return Ok(Vec::new());
            }
            let data_id = selection.data_id;
            let id = Self::allocate(&mut state, amount as usize)?;
            let mut fetch = ChunkedFetch::new(
                id,
                index,
                amount,
                self.max_chunk,
                data_id,
                u64::from(MAX_READ),
            );
            let mut request = fetch.next_request().ok_or(())?;
            request.position = position;
            self.submit(
                &mut state,
                generation,
                request,
                Operation::Range {
                    base: position,
                    fetch,
                },
                amount as usize,
            )?
        };
        match receiver.await.map_err(|_| ())?? {
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err(()),
        }
    }

    fn allocate(state: &mut State, bytes: usize) -> Result<u32, ()> {
        // Prune consumers that have gone away before applying either budget.
        state
            .pending
            .retain(|_, pending| !pending.answer.is_closed());
        if state.pending.len() >= MAX_PENDING
            || bytes + state.pending.values().map(|p| p.reserved).sum::<usize>() > MAX_BUFFERED
        {
            return Err(());
        }
        let id = state.next_stream.ok_or(())?;
        state.next_stream = id.checked_add(1); // A late response can never alias a new request.
        Ok(id)
    }

    fn submit(
        &self,
        state: &mut State,
        generation: u64,
        request: FileContentsRequest,
        operation: Operation,
        reserved: usize,
    ) -> Result<oneshot::Receiver<Result<Value, ()>>, ()> {
        let id = request.stream_id;
        let (answer, receiver) = oneshot::channel();
        let (cancel, cancelled) = oneshot::channel();
        self.sender
            .send(ServerEvent::Clipboard(
                ClipboardMessage::SendFileContentsRequest(request),
            ))
            .map_err(|_| ())?;
        state.pending.insert(
            id,
            Pending {
                generation,
                operation,
                answer,
                reserved,
                _cancel_timeout: cancel,
            },
        );
        let shared = Arc::clone(&self.state);
        let timeout = self.timeout;
        self.runtime.spawn(async move {
            tokio::select! {
                _ = cancelled => {},
                _ = tokio::time::sleep(timeout) => {
                    if let Ok(mut state) = shared.lock() { state.pending.remove(&id); }
                }
            }
        });
        Ok(receiver)
    }

    pub(super) fn on_response(&self, response: FileContentsResponse<'_>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(mut pending) = state.pending.remove(&response.stream_id()) else {
            return;
        };
        if pending.answer.is_closed() {
            return;
        }
        let generation = pending.generation;
        let huge = state.huge;
        let result = match &mut pending.operation {
            Operation::Size { inode } => {
                if response.is_error() {
                    Err(())
                } else if let Ok(bytes) = <[u8; 8]>::try_from(response.data()) {
                    let size = u64::from_le_bytes(bytes);
                    let stored = state
                        .selection_mut(generation)
                        .is_some_and(|selection| selection.tree.set_size(*inode, size));
                    if (!huge && size > u64::from(u32::MAX)) || !stored {
                        Err(())
                    } else {
                        Ok(Value::Size(size))
                    }
                } else {
                    Err(())
                }
            }
            Operation::Range { base, fetch } => match fetch.on_response(&response) {
                ChunkedFetchProgress::Failed => Err(()),
                ChunkedFetchProgress::Complete => {
                    let Operation::Range { fetch, .. } = pending.operation else {
                        unreachable!()
                    };
                    let _ = pending.answer.send(Ok(Value::Bytes(fetch.into_data())));
                    return;
                }
                ChunkedFetchProgress::InProgress => {
                    if let Some(mut request) = fetch.next_request() {
                        if let Some(position) = base.checked_add(request.position) {
                            request.position = position;
                            if self
                                .sender
                                .send(ServerEvent::Clipboard(
                                    ClipboardMessage::SendFileContentsRequest(request),
                                ))
                                .is_ok()
                            {
                                state.pending.insert(response.stream_id(), pending);
                                return;
                            }
                        }
                    }
                    Err(())
                }
            },
        };
        let _ = pending.answer.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn generated_inbound_replacement_sequences_do_not_reuse_requests(
            replacements in proptest::collection::vec(any::<bool>(), 0..24),
        ) {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let (transfer, mut events, mut generation, mut inode) = setup(&[FileDescriptor::new("same-name").with_file_size(1)], 1);
                let mut last_stream = 0;
                for replace in replacements {
                    let pending = read(&transfer, generation, inode, 0, 1);
                    let req = request(&mut events).await;
                    assert!(req.stream_id > last_stream);
                    last_stream = req.stream_id;
                    if replace {
                        let (next, _) = transfer.accept(&[FileDescriptor::new("same-name").with_file_size(1)], Some(last_stream)).unwrap();
                        // Superseded, not ended: the lock still covers its bytes,
                        // so it keeps serving while Wayland advertises the new one.
                        assert!(transfer.with_tree(generation, |_| ()).is_some());
                        assert!(transfer.with_live_tree(generation, |_| ()).is_none());
                        generation = next;
                        inode = transfer.with_tree(generation, |tree| tree.roots()[0]).unwrap();
                    }
                    transfer.on_response(FileContentsResponse::new_data_response(req.stream_id, b"x".to_vec()));
                    // A read issued before the replacement completes either way.
                    assert_eq!(pending.await.unwrap().unwrap(), b"x");
                    assert!(events.try_recv().is_err());
                }
                transfer.close();
                transfer.capabilities(true, true, true);
                assert!(!transfer.enabled());
                assert!(transfer.read(generation, inode, 0, 1).await.is_err());
            });
        }
    }

    fn setup(
        files: &[FileDescriptor],
        chunk: u32,
    ) -> (Transfer, mpsc::UnboundedReceiver<ServerEvent>, u64, u64) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let transfer = Transfer::new(sender, 100, chunk);
        transfer.capabilities(true, true, true);
        let (generation, _) = transfer.accept(files, Some(77)).unwrap();
        let inode = transfer
            .with_tree(generation, |tree| tree.roots()[0])
            .unwrap();
        (transfer, receiver, generation, inode)
    }

    async fn request(receiver: &mut mpsc::UnboundedReceiver<ServerEvent>) -> FileContentsRequest {
        let Some(ServerEvent::Clipboard(ClipboardMessage::SendFileContentsRequest(request))) =
            tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
        else {
            panic!("expected file contents request")
        };
        request
    }

    fn read(
        transfer: &Transfer,
        generation: u64,
        inode: u64,
        position: u64,
        size: u32,
    ) -> tokio::task::JoinHandle<Result<Vec<u8>, ()>> {
        let transfer = transfer.clone();
        tokio::spawn(async move { transfer.read(generation, inode, position, size).await })
    }

    #[tokio::test]
    async fn inbound_known_and_unknown_size_reads() {
        for size in [Some(5), None] {
            let mut file = FileDescriptor::new("file");
            file.file_size = size;
            let (transfer, mut events, generation, inode) = setup(&[file], 100);
            let read = read(&transfer, generation, inode, 1, 100);
            if size.is_none() {
                let req = request(&mut events).await;
                assert_eq!(req.flags, FileContentsFlags::SIZE);
                assert_eq!(
                    (req.position, req.requested_size, req.data_id),
                    (0, 8, Some(77))
                );
                transfer.on_response(FileContentsResponse::new_size_response(req.stream_id, 5));
            }
            let req = request(&mut events).await;
            assert_eq!(req.flags, FileContentsFlags::RANGE);
            assert_eq!(
                (req.index, req.position, req.requested_size, req.data_id),
                (0, 1, 4, Some(77))
            );
            transfer.on_response(FileContentsResponse::new_data_response(
                req.stream_id,
                b"BCDE".to_vec(),
            ));
            assert_eq!(read.await.unwrap().unwrap(), b"BCDE");
            assert!(transfer
                .read(generation, inode, 5, 10)
                .await
                .unwrap()
                .is_empty());
            assert!(events.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn inbound_short_range_progress() {
        let (transfer, mut events, generation, inode) =
            setup(&[FileDescriptor::new("file").with_file_size(100)], 3);
        let read = read(&transfer, generation, inode, 20, 7);
        for (offset, requested, data) in
            [(20, 3, b"ab".as_slice()), (22, 3, b"cde"), (25, 2, b"fg")]
        {
            let req = request(&mut events).await;
            assert_eq!((req.position, req.requested_size), (offset, requested));
            transfer.on_response(FileContentsResponse::new_data_response(
                req.stream_id,
                data.to_vec(),
            ));
        }
        assert_eq!(read.await.unwrap().unwrap(), b"abcdefg");
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_replaced_selection_serves_until_its_lock_is_released() {
        let (transfer, mut events, generation, inode) =
            setup(&[FileDescriptor::new("old").with_file_size(5)], 5);
        let pending = read(&transfer, generation, inode, 0, 5);
        let old = request(&mut events).await;

        // A second selection arrives. The first is superseded, but the client
        // is still holding its file data under lock 77.
        let (new_generation, _, released) = transfer
            .accept_for(
                generation,
                &[FileDescriptor::new("new").with_file_size(5)],
                Some(78),
            )
            .unwrap();
        assert!(
            released.is_empty(),
            "a superseded selection under lock keeps its mount"
        );
        assert!(transfer.with_tree(generation, |_| ()).is_some());
        assert!(
            transfer.with_live_tree(generation, |_| ()).is_none(),
            "Wayland must advertise only the newest selection"
        );

        // The read issued before the replacement still completes.
        transfer.on_response(FileContentsResponse::new_data_response(
            old.stream_id,
            b"right".to_vec(),
        ));
        assert_eq!(pending.await.unwrap().unwrap(), b"right");

        // An inode is never reused for a different file.
        assert!(transfer.read(new_generation, inode, 0, 5).await.is_err());

        // The Unlock for 77 goes out: the client may drop those bytes, so the
        // retired selection stops serving and its mount comes down.
        let stranded = read(&transfer, generation, inode, 0, 5);
        let _ = request(&mut events).await;
        assert_eq!(transfer.release_locks(&[77]), vec![generation]);
        assert!(stranded.await.unwrap().is_err());
        assert!(transfer.with_tree(generation, |_| ()).is_none());
        assert!(transfer.read(generation, inode, 0, 5).await.is_err());

        // Closing ends the live selection too.
        transfer.close();
        assert!(transfer
            .accept(&[FileDescriptor::new("closed")], None)
            .is_none());
        assert!(transfer.read(new_generation, inode, 0, 5).await.is_err());
    }

    /// Without a lock there is nothing keeping the client's copy alive, so a
    /// replacement ends the old selection outright -- the behaviour every client
    /// got before locking, and the one a client that declines CAN_LOCK_CLIPDATA
    /// still gets.
    #[tokio::test]
    async fn an_unlocked_selection_is_ended_by_its_replacement() {
        let (sender, mut events) = mpsc::unbounded_channel();
        let transfer = Transfer::new(sender, 100, 5);
        transfer.capabilities(true, true, true);
        let (generation, _) = transfer
            .accept(&[FileDescriptor::new("old").with_file_size(5)], None)
            .unwrap();
        let inode = transfer
            .with_tree(generation, |tree| tree.roots()[0])
            .unwrap();
        let pending = read(&transfer, generation, inode, 0, 5);
        let _ = request(&mut events).await;

        let (_, _, released) = transfer
            .accept_for(
                generation,
                &[FileDescriptor::new("new").with_file_size(5)],
                None,
            )
            .unwrap();
        assert_eq!(
            released,
            vec![generation],
            "an unlocked selection's mount must come down with it"
        );
        assert!(pending.await.unwrap().is_err());
        assert!(transfer.with_tree(generation, |_| ()).is_none());
    }

    #[tokio::test]
    async fn released_live_lock_cancels_reads_and_cannot_be_retired() {
        let (transfer, mut events, generation, inode) =
            setup(&[FileDescriptor::new("old").with_file_size(5)], 5);
        let pending = read(&transfer, generation, inode, 0, 5);
        let old = request(&mut events).await;
        assert_eq!(transfer.release_locks(&[77]), vec![generation]);
        assert!(pending.await.unwrap().is_err());
        assert!(transfer.with_tree(generation, |_| ()).is_none());
        transfer.retire_generation();
        assert!(transfer.with_tree(generation, |_| ()).is_none());
        transfer.on_response(FileContentsResponse::new_data_response(
            old.stream_id,
            b"stale".to_vec(),
        ));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn retired_selection_ceiling_and_shutdown_bound_all_generations() {
        let (transfer, _events, first, _) =
            setup(&[FileDescriptor::new("old").with_file_size(5)], 5);
        let mut generations = vec![first];
        for id in 100..110 {
            let (generation, _) = transfer
                .accept(&[FileDescriptor::new("new").with_file_size(5)], Some(id))
                .unwrap();
            generations.push(generation);
            assert!(transfer.state.lock().unwrap().retiring.len() <= MAX_RETIRING);
        }
        assert!(transfer.with_tree(first, |_| ()).is_none());
        transfer.capabilities(true, true, false);
        for generation in generations {
            assert!(transfer.with_tree(generation, |_| ()).is_none());
        }
        assert!(transfer.state.lock().unwrap().retiring.is_empty());
    }

    #[tokio::test]
    async fn inbound_timeout_and_response_bounds() {
        let (mut transfer, mut events, generation, inode) =
            setup(&[FileDescriptor::new("file").with_file_size(10)], 4);
        for data in [b"12345".as_slice(), b""] {
            let pending = read(&transfer, generation, inode, 0, 4);
            let req = request(&mut events).await;
            transfer.on_response(FileContentsResponse::new_data_response(
                req.stream_id + 1,
                b"late".to_vec(),
            ));
            assert!(!pending.is_finished());
            transfer.on_response(FileContentsResponse::new_data_response(
                req.stream_id,
                data.to_vec(),
            ));
            assert!(pending.await.unwrap().is_err());
        }
        transfer.timeout = Duration::from_millis(10);
        let pending = read(&transfer, generation, inode, 0, 4);
        let req = request(&mut events).await;
        assert!(tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        transfer.on_response(FileContentsResponse::new_data_response(
            req.stream_id,
            b"late".to_vec(),
        ));
        assert!(transfer.state.lock().unwrap().pending.is_empty());
        drop(events);
        assert!(transfer.read(generation, inode, 0, 4).await.is_err());
    }

    #[tokio::test]
    async fn inbound_pending_budget() {
        let (transfer, mut events, generation, inode) = setup(
            &[FileDescriptor::new("file").with_file_size(u64::from(MAX_READ))],
            4,
        );
        let mut held = Vec::new();
        for _ in 0..MAX_PENDING {
            held.push(read(&transfer, generation, inode, 0, 4));
            request(&mut events).await;
        }
        assert!(transfer.read(generation, inode, 0, 4).await.is_err());
        assert!(events.try_recv().is_err());
        transfer.invalidate();
        for pending in held {
            assert!(pending.await.unwrap().is_err());
        }

        let (generation, _) = transfer
            .accept(
                &[FileDescriptor::new("large").with_file_size(u64::from(MAX_READ))],
                None,
            )
            .unwrap();
        let inode = transfer
            .with_tree(generation, |tree| tree.roots()[0])
            .unwrap();
        let one = read(&transfer, generation, inode, 0, MAX_READ);
        request(&mut events).await;
        let two = read(&transfer, generation, inode, 0, MAX_READ);
        request(&mut events).await;
        assert!(transfer.read(generation, inode, 0, 1).await.is_err());
        one.abort();
        assert!(one.await.unwrap_err().is_cancelled());
        let recovered = read(&transfer, generation, inode, 0, 1);
        let req = request(&mut events).await;
        transfer.on_response(FileContentsResponse::new_data_response(
            req.stream_id,
            b"x".to_vec(),
        ));
        assert_eq!(recovered.await.unwrap().unwrap(), b"x");
        transfer.close();
        assert!(two.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn inbound_size_failure_and_stream_exhaustion_are_terminal_for_the_request() {
        let (transfer, mut events, generation, inode) = setup(&[FileDescriptor::new("unknown")], 4);
        let pending = read(&transfer, generation, inode, 0, 1);
        let req = request(&mut events).await;
        transfer.on_response(FileContentsResponse::new_data_response(
            req.stream_id,
            vec![0; 7],
        ));
        assert!(pending.await.unwrap().is_err());
        transfer.state.lock().unwrap().next_stream = Some(u32::MAX);
        let pending = read(&transfer, generation, inode, 0, 1);
        let req = request(&mut events).await;
        assert_eq!(req.stream_id, u32::MAX);
        transfer.on_response(FileContentsResponse::new_error(req.stream_id));
        assert!(pending.await.unwrap().is_err());
        assert!(transfer.read(generation, inode, 0, 1).await.is_err());
        assert!(events.try_recv().is_err());
    }
}
