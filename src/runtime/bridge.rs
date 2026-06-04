use crate::buffer::pool::BufferPool;
use crate::io::{AlignedBuffer, FileHandle, FileSystem};
use crate::storage::page::{PageId, PAGE_SIZE};
use crate::wal::writer::WalWriter;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{spawn, JoinHandle};

/// Open the data file once per worker, then reuse the resulting [`FileHandle`]
/// for every page read/write.  This eliminates the per-request `open()` syscall
/// that the bridge previously incurred.
fn open_data_handle(
    fs: &dyn FileSystem,
    path: &std::path::Path,
    open_count: &AtomicU64,
    cached: &mut Option<Box<dyn FileHandle>>,
) -> std::io::Result<()> {
    if cached.is_none() {
        let h = fs.open(path, false)?;
        open_count.fetch_add(1, Ordering::Relaxed);
        *cached = Some(h);
    }
    Ok(())
}

/// A command sent from the async/sync frontend to the storage backend.
#[derive(Debug)]
pub enum IoCommand {
    /// Read a page from disk into a fresh buffer.
    ReadPage { page_id: PageId, respond: Sender<IoResponse> },
    /// Write a page image to disk.
    WritePage { page_id: PageId, buf: AlignedBuffer, respond: Sender<IoResponse> },
    /// Flush the WAL to durable storage.
    SyncWal { respond: Sender<IoResponse> },
    /// Flush a specific dirty page back to disk.
    FlushPage { page_id: PageId, respond: Sender<IoResponse> },
    /// Shutdown the worker thread.
    Shutdown,
}

/// Response returned by the storage backend.
#[derive(Debug)]
pub enum IoResponse {
    /// Operation completed successfully.  For `ReadPage` this contains
    /// the page data (PAGE_SIZE bytes).
    Ok(Vec<u8>),
    /// An I/O or storage error occurred.
    Err(String),
}

/// Handle held by the frontend (async task or sync caller).  Dropping it
/// does **not** stop the bridge; call [`IoBridge::stop`] for that.
pub struct IoHandle {
    pub tx: Sender<IoCommand>,
}

impl IoHandle {
    /// Send a command and block until the response arrives.
    ///
    /// Each request-bearing command is dispatched through the corresponding
    /// typed helper, which owns a fresh response channel — so this method never
    /// reaches an `unreachable!()` and never panics on a request.  If the bridge
    /// worker has exited, an [`IoResponse::Err`] is returned rather than
    /// panicking.
    pub fn call_sync(&self, cmd: IoCommand) -> IoResponse {
        match cmd {
            // The respond channel embedded in the caller-supplied command cannot
            // be recovered once the command is moved, so we discard it and
            // delegate to the typed helper, which manages its own channel.
            IoCommand::ReadPage { page_id, .. } => self.read_page_sync(page_id),
            IoCommand::WritePage { page_id, buf, .. } => self.write_page_sync(page_id, buf),
            IoCommand::SyncWal { .. } => self.sync_wal_sync(),
            IoCommand::FlushPage { page_id, .. } => self.flush_page_sync(page_id),
            IoCommand::Shutdown => {
                self.tx.send(IoCommand::Shutdown).ok();
                IoResponse::Ok(vec![])
            }
        }
    }

    /// Convenience: read a page synchronously.
    pub fn read_page_sync(&self, page_id: PageId) -> IoResponse {
        let (tx, rx) = bounded(1);
        if self
            .tx
            .send(IoCommand::ReadPage {
                page_id,
                respond: tx,
            })
            .is_err()
        {
            return IoResponse::Err("bridge worker unavailable".into());
        }
        Self::recv_response(rx)
    }

    /// Convenience: write a page synchronously.
    pub fn write_page_sync(&self, page_id: PageId, buf: AlignedBuffer) -> IoResponse {
        let (tx, rx) = bounded(1);
        if self
            .tx
            .send(IoCommand::WritePage {
                page_id,
                buf,
                respond: tx,
            })
            .is_err()
        {
            return IoResponse::Err("bridge worker unavailable".into());
        }
        Self::recv_response(rx)
    }

    /// Convenience: sync WAL synchronously.
    pub fn sync_wal_sync(&self) -> IoResponse {
        let (tx, rx) = bounded(1);
        if self.tx.send(IoCommand::SyncWal { respond: tx }).is_err() {
            return IoResponse::Err("bridge worker unavailable".into());
        }
        Self::recv_response(rx)
    }

    /// Convenience: flush a specific page synchronously.
    pub fn flush_page_sync(&self, page_id: PageId) -> IoResponse {
        let (tx, rx) = bounded(1);
        if self
            .tx
            .send(IoCommand::FlushPage {
                page_id,
                respond: tx,
            })
            .is_err()
        {
            return IoResponse::Err("bridge worker unavailable".into());
        }
        Self::recv_response(rx)
    }

    /// Receive a response, mapping a disconnected channel to a typed error
    /// instead of panicking.
    fn recv_response(rx: Receiver<IoResponse>) -> IoResponse {
        rx.recv()
            .unwrap_or_else(|_| IoResponse::Err("bridge worker terminated before responding".into()))
    }
}

/// The bridge owns a pool of worker threads that execute I/O commands
/// against the shared [`BufferPool`] and [`WalWriter`].
pub struct IoBridge {
    /// Handle exposed to callers.
    pub handle: IoHandle,
    /// Signal to workers that they should exit.
    shutdown: Arc<AtomicBool>,
    /// Thread handles.
    workers: Vec<JoinHandle<()>>,
    /// Total number of times a worker opened the data file.  With persistent
    /// fds this is at most `worker_count` for the bridge's whole lifetime.
    data_open_count: Arc<AtomicU64>,
}

impl IoBridge {
    /// Start `worker_count` background threads that pull commands from a
    /// single bounded channel and execute them against `pool` and `wal`.
    pub fn new(
        pool: Arc<BufferPool>,
        fs: Arc<dyn FileSystem>,
        wal: Arc<std::sync::Mutex<WalWriter>>,
        worker_count: usize,
        channel_capacity: usize,
    ) -> Self {
        let (tx, rx): (Sender<IoCommand>, Receiver<IoCommand>) = bounded(channel_capacity);
        let shutdown = Arc::new(AtomicBool::new(false));
        let data_open_count = Arc::new(AtomicU64::new(0));
        let mut workers = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let rx = rx.clone();
            let shutdown = shutdown.clone();
            let pool = pool.clone();
            let fs = fs.clone();
            let wal = wal.clone();
            let open_count = data_open_count.clone();
            workers.push(spawn(move || {
                // Persistent, lazily-opened data-file handle reused across all
                // requests this worker serves.
                let mut data_handle: Option<Box<dyn FileHandle>> = None;

                while !shutdown.load(Ordering::Relaxed) {
                    match rx.recv_timeout(std::time::Duration::from_millis(10)) {
                        Ok(IoCommand::ReadPage { page_id, respond }) => {
                            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
                            let offset = page_id * PAGE_SIZE as u64;
                            let res = match open_data_handle(
                                fs.as_ref(),
                                &pool.data_path,
                                &open_count,
                                &mut data_handle,
                            ) {
                                Ok(()) => {
                                    let handle = data_handle
                                        .as_deref()
                                        .expect("INVARIANT: handle opened above");
                                    match handle.read_at(&mut buf, offset) {
                                        Ok(()) => IoResponse::Ok(buf.to_vec()),
                                        Err(e) => IoResponse::Err(e.to_string()),
                                    }
                                }
                                Err(e) => IoResponse::Err(e.to_string()),
                            };
                            let _ = respond.send(res);
                        }
                        Ok(IoCommand::WritePage { page_id, buf, respond }) => {
                            let offset = page_id * PAGE_SIZE as u64;
                            let res = match open_data_handle(
                                fs.as_ref(),
                                &pool.data_path,
                                &open_count,
                                &mut data_handle,
                            ) {
                                Ok(()) => {
                                    let handle = data_handle
                                        .as_deref()
                                        .expect("INVARIANT: handle opened above");
                                    match handle.write_at(&buf, offset) {
                                        Ok(()) => match handle.sync_data() {
                                            Ok(()) => IoResponse::Ok(vec![]),
                                            Err(e) => IoResponse::Err(e.to_string()),
                                        },
                                        Err(e) => IoResponse::Err(e.to_string()),
                                    }
                                }
                                Err(e) => IoResponse::Err(e.to_string()),
                            };
                            let _ = respond.send(res);
                        }
                        Ok(IoCommand::SyncWal { respond }) => {
                            let res = match wal.lock() {
                                Ok(mut w) => match w.sync(fs.as_ref()) {
                                    Ok(()) => IoResponse::Ok(vec![]),
                                    Err(e) => IoResponse::Err(e.to_string()),
                                },
                                Err(_) => IoResponse::Err("wal mutex poisoned".into()),
                            };
                            let _ = respond.send(res);
                        }
                        Ok(IoCommand::FlushPage { page_id, respond }) => {
                            // Find the frame holding this page and flush it.
                            let mut found = false;
                            let mut res = IoResponse::Ok(vec![]);
                            for (fid, frame) in pool.iter_frames().enumerate() {
                                if frame.desc.page_id.load(Ordering::Relaxed) == page_id {
                                    if let Err(e) = pool.flush_single_frame(fs.as_ref(), fid as u32) {
                                        res = IoResponse::Err(e.to_string());
                                    }
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                res = IoResponse::Err(format!("page {} not in pool", page_id));
                            }
                            let _ = respond.send(res);
                        }
                        Ok(IoCommand::Shutdown) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            }));
        }

        Self {
            handle: IoHandle { tx },
            shutdown,
            workers,
            data_open_count,
        }
    }

    /// Total number of `open()` calls workers have made on the data file.
    ///
    /// With persistent fds this saturates at the worker count: each worker
    /// opens the data file at most once for its whole lifetime, regardless of
    /// how many page requests it serves.
    pub fn data_open_count(&self) -> u64 {
        self.data_open_count.load(Ordering::Relaxed)
    }

    /// Signal workers to stop and wait for them.
    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.handle.tx.send(IoCommand::Shutdown);
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::pool::BufferPool;
    use crate::io::posix::PosixFileSystem;
    use std::io::Write;

    fn setup_bridge(
        frames: u32,
    ) -> (
        tempfile::TempDir,
        IoBridge,
        Arc<BufferPool>,
        Arc<std::sync::Mutex<WalWriter>>,
    ) {
        use crate::storage::page::{PageType, SlottedPage};
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("rgraph.db");
        let wal_dir = dir.path().join("wal");
        let fs = Arc::new(PosixFileSystem::new(false));
        let file_pages = (frames as u64).max(64);
        {
            let mut f = std::fs::File::create(&data_path).unwrap();
            f.set_len(file_pages * PAGE_SIZE as u64).unwrap();
            f.flush().unwrap();
        }
        // Initialize all pages with valid checksums.
        let handle = fs.open(&data_path, false).unwrap();
        for pid in 0..file_pages {
            let mut page = SlottedPage::init(pid, PageType::SlottedData);
            page.update_checksum();
            handle.write_at(&page.buf, pid * PAGE_SIZE as u64).unwrap();
        }
        handle.sync_data().unwrap();
        drop(handle);

        let pool = Arc::new(BufferPool::new(frames, data_path));
        let wal = Arc::new(std::sync::Mutex::new(
            WalWriter::open(wal_dir, fs.as_ref()).unwrap(),
        ));
        let bridge = IoBridge::new(pool.clone(), fs.clone(), wal.clone(), 2, 64);
        (dir, bridge, pool, wal)
    }

    #[test]
    fn bridge_read_page() {
        let (_dir, bridge, pool, _wal) = setup_bridge(4);
        // Seed page 1 through the pool so it exists in memory and on disk.
        {
            let mut guard = pool.fix_page(
                &PosixFileSystem::new(false), 1).unwrap();
            println!("After fix: frame_id={}, page_id={}, dirty={}",
                guard.frame_id,
                guard.page_id,
                guard.desc().dirty.load(std::sync::atomic::Ordering::Relaxed));
            guard.buf_mut()[0] = 0xAB;
            guard.buf_mut()[1] = 0xCD;
            guard.set_dirty(0);
            println!("After set_dirty: dirty={}",
                guard.desc().dirty.load(std::sync::atomic::Ordering::Relaxed));
        }
        pool.flush_all(&PosixFileSystem::new(false)).unwrap();
        println!("After flush_all");
        for (i, frame) in pool.iter_frames().enumerate() {
            println!("Frame {}: page_id={}, dirty={}, buf[0]={}",
                i,
                frame.desc.page_id.load(std::sync::atomic::Ordering::Relaxed),
                frame.desc.dirty.load(std::sync::atomic::Ordering::Relaxed),
                frame.buf[0]);
        }

        let res = bridge.handle.read_page_sync(1);
        match res {
            IoResponse::Ok(data) => {
                assert_eq!(data[0], 0xAB);
                assert_eq!(data[1], 0xCD);
            }
            IoResponse::Err(e) => panic!("read failed: {}", e),
        }
        bridge.stop();
    }

    #[test]
    fn bridge_write_and_read_roundtrip() {
        let (_dir, bridge, _pool, _wal) = setup_bridge(4);
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        buf[0] = 0xCA;
        buf[1] = 0xFE;
        let res = bridge.handle.write_page_sync(2, buf);
        assert!(matches!(res, IoResponse::Ok(_)), "write failed");

        let res = bridge.handle.read_page_sync(2);
        match res {
            IoResponse::Ok(data) => {
                assert_eq!(data[0], 0xCA);
                assert_eq!(data[1], 0xFE);
            }
            IoResponse::Err(e) => panic!("read failed: {}", e),
        }
        bridge.stop();
    }

    #[test]
    fn bridge_sync_wal() {
        let (_dir, bridge, _pool, _wal) = setup_bridge(4);
        let res = bridge.handle.sync_wal_sync();
        assert!(matches!(res, IoResponse::Ok(_)), "sync failed");
        bridge.stop();
    }

    #[test]
    fn bridge_persistent_fd_does_not_reopen_per_request() {
        // Use a single worker so the data-file open count is unambiguous, then
        // serve many reads/writes and assert the file was opened at most once.
        use crate::storage::page::{PageType, SlottedPage};
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("rgraph.db");
        let wal_dir = dir.path().join("wal");
        let fs = Arc::new(PosixFileSystem::new(false));
        {
            let mut f = std::fs::File::create(&data_path).unwrap();
            f.set_len(64 * PAGE_SIZE as u64).unwrap();
            f.flush().unwrap();
            let handle = fs.open(&data_path, false).unwrap();
            for pid in 0..64u64 {
                let mut page = SlottedPage::init(pid, PageType::SlottedData);
                page.update_checksum();
                handle.write_at(&page.buf, pid * PAGE_SIZE as u64).unwrap();
            }
            handle.sync_data().unwrap();
        }
        let pool = Arc::new(BufferPool::new(4, data_path));
        let wal = Arc::new(std::sync::Mutex::new(
            WalWriter::open(wal_dir, fs.as_ref()).unwrap(),
        ));
        let bridge = IoBridge::new(pool.clone(), fs.clone(), wal.clone(), 1, 64);

        for i in 0..40u64 {
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            buf[0] = (i & 0xFF) as u8;
            assert!(matches!(
                bridge.handle.write_page_sync(5, buf),
                IoResponse::Ok(_)
            ));
            assert!(matches!(bridge.handle.read_page_sync(5), IoResponse::Ok(_)));
        }

        assert!(
            bridge.data_open_count() <= 1,
            "data file must be opened at most once per worker, got {}",
            bridge.data_open_count()
        );
        bridge.stop();
    }

    #[test]
    fn call_sync_dispatches_all_request_variants_without_unreachable() {
        // Regression for Task 177: call_sync must delegate each request-bearing
        // command to its typed helper instead of hitting `unreachable!()`.
        let (_dir, bridge, _pool, _wal) = setup_bridge(4);

        // WritePage then ReadPage round-trip via the generic call_sync entry.
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        buf[0] = 0x42;
        let (resp_tx, _resp_rx) = bounded(1);
        let write = bridge.handle.call_sync(IoCommand::WritePage {
            page_id: 3,
            buf,
            respond: resp_tx,
        });
        assert!(matches!(write, IoResponse::Ok(_)), "write via call_sync");

        let (resp_tx, _resp_rx) = bounded(1);
        let read = bridge.handle.call_sync(IoCommand::ReadPage {
            page_id: 3,
            respond: resp_tx,
        });
        match read {
            IoResponse::Ok(data) => assert_eq!(data[0], 0x42),
            IoResponse::Err(e) => panic!("read via call_sync failed: {e}"),
        }

        let (resp_tx, _resp_rx) = bounded(1);
        let sync = bridge
            .handle
            .call_sync(IoCommand::SyncWal { respond: resp_tx });
        assert!(matches!(sync, IoResponse::Ok(_)), "sync wal via call_sync");

        let shutdown = bridge.handle.call_sync(IoCommand::Shutdown);
        assert!(matches!(shutdown, IoResponse::Ok(_)));

        bridge.stop();
    }

    #[test]
    fn call_sync_on_dead_bridge_returns_error_not_panic() {
        // After the bridge workers stop, call_sync must surface a typed error
        // rather than panicking on the closed channel.
        let (_dir, bridge, _pool, _wal) = setup_bridge(4);
        let handle_tx = bridge.handle.tx.clone();
        bridge.stop();

        let dead = IoHandle { tx: handle_tx };
        let (resp_tx, _resp_rx) = bounded(1);
        let res = dead.call_sync(IoCommand::ReadPage {
            page_id: 1,
            respond: resp_tx,
        });
        assert!(matches!(res, IoResponse::Err(_)));
    }
}
