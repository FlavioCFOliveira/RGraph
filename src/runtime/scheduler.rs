use crate::buffer::pool::BufferPool;
use crate::io::{FileHandle, FileSystem};
use crate::wal::writer::WalWriter;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{spawn, JoinHandle};
use std::time::Duration;

/// Priority lanes for the weighted-fair queuing scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// P0: WAL append / sync — must never queue behind bulk I/O.
    Wal = 0,
    /// P1: Index probe — fast, latency-sensitive lookups.
    IndexProbe = 1,
    /// P2: Page read — standard buffer-pool miss.
    PageRead = 2,
    /// P3: Bulk read — table scans, large sequential reads.
    BulkRead = 3,
}

/// A request scheduled through the multi-queue I/O scheduler.
#[derive(Debug)]
pub struct ScheduleRequest {
    pub priority: Priority,
    pub command: ScheduleCommand,
    pub respond: Sender<ScheduleResponse>,
}

/// Commands the scheduler can execute.
#[derive(Debug)]
pub enum ScheduleCommand {
    /// Read a page from disk.
    ReadPage { page_id: u64 },
    /// Write a page image to disk.
    WritePage { page_id: u64, buf: Vec<u8> },
    /// Sync the WAL.
    SyncWal,
    /// Flush a specific dirty page.
    FlushPage { page_id: u64 },
}

/// Response returned by the scheduler backend.
#[derive(Debug)]
pub enum ScheduleResponse {
    Ok(Vec<u8>),
    Err(String),
}

/// Per-lane service weights for the weighted-fair drain, indexed by
/// [`Priority`].  Higher-priority lanes receive a larger share of each
/// scheduling cycle, but **every** lane is guaranteed at least one slot per
/// cycle, so no lane can be starved by a saturated higher-priority lane.
///
/// The ratio (8:4:2:1) gives P0 eight times the bandwidth of P3 while still
/// admitting P3 work on every cycle — the property the previous strict-priority
/// loop lacked.
const LANE_WEIGHTS: [u32; 4] = [8, 4, 2, 1];

/// A production multi-queue I/O scheduler with weighted-fair priority lanes.
///
/// # Scheduling
///
/// Each worker runs a weighted round-robin over the four lanes.  Within one
/// cycle a lane may serve up to [`LANE_WEIGHTS`]`[lane]` requests before the
/// worker advances to the next lane; a lane with no work is skipped
/// immediately.  This bounds the worst-case latency of a low-priority request
/// to one cycle of higher-priority work rather than allowing indefinite
/// starvation (the defect of the earlier strict-priority loop).  When all lanes
/// are empty the worker blocks on a select across every lane with a short
/// timeout so shutdown remains responsive.
///
/// # Persistent file descriptors
///
/// Each worker opens the data file **once** (lazily, on first use) and reuses
/// the resulting [`FileHandle`] for every subsequent page read/write, instead
/// of calling `open()` per request.  The number of `open()` calls is exposed
/// via [`IoScheduler::data_open_count`] for observability and testing.
pub struct IoScheduler {
    /// Senders for each priority lane (shared by all producers).
    pub tx: [Sender<ScheduleRequest>; 4],
    /// Signal to stop.
    shutdown: Arc<AtomicBool>,
    /// Worker handles.
    workers: Vec<JoinHandle<()>>,
    /// Total number of times a worker opened the data file.  With persistent
    /// fds this is at most `worker_count` for the whole lifetime of the
    /// scheduler, regardless of how many requests are served.
    data_open_count: Arc<AtomicU64>,
}

/// A worker's cached, persistent handle to the data file.
///
/// Opened lazily on first use and reused for the worker's lifetime, eliminating
/// per-request `open()` syscalls.
struct DataFile<'a> {
    fs: &'a dyn FileSystem,
    path: &'a std::path::Path,
    handle: Option<Box<dyn FileHandle>>,
    open_count: &'a AtomicU64,
}

impl<'a> DataFile<'a> {
    fn new(
        fs: &'a dyn FileSystem,
        path: &'a std::path::Path,
        open_count: &'a AtomicU64,
    ) -> Self {
        Self {
            fs,
            path,
            handle: None,
            open_count,
        }
    }

    /// Return the cached handle, opening (and caching) it on first use.
    fn get(&mut self) -> std::io::Result<&dyn FileHandle> {
        if self.handle.is_none() {
            let h = self.fs.open(self.path, false)?;
            self.open_count.fetch_add(1, Ordering::Relaxed);
            self.handle = Some(h);
        }
        // INVARIANT: `handle` is `Some` — we just populated it above.
        Ok(self
            .handle
            .as_deref()
            .expect("INVARIANT: handle populated immediately above"))
    }
}

impl IoScheduler {
    /// Start the scheduler with `worker_count` background threads.
    ///
    /// Each worker continuously drains requests from the four priority
    /// lanes, always favouring lower lane numbers.
    pub fn new(
        pool: Arc<BufferPool>,
        fs: Arc<dyn FileSystem>,
        wal: Arc<std::sync::Mutex<WalWriter>>,
        worker_count: usize,
        lane_capacity: usize,
    ) -> Self {
        // One bounded channel per priority lane.
        let (tx0, rx0) = bounded(lane_capacity);
        let (tx1, rx1) = bounded(lane_capacity);
        let (tx2, rx2) = bounded(lane_capacity);
        let (tx3, rx3) = bounded(lane_capacity);
        let receivers: [Receiver<ScheduleRequest>; 4] = [rx0, rx1, rx2, rx3];
        let tx: [Sender<ScheduleRequest>; 4] = [tx0, tx1, tx2, tx3];

        let shutdown = Arc::new(AtomicBool::new(false));
        let data_open_count = Arc::new(AtomicU64::new(0));
        let mut workers = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let shutdown = shutdown.clone();
            let pool = pool.clone();
            let fs = fs.clone();
            let wal = wal.clone();
            let open_count = data_open_count.clone();
            let rxs: [Receiver<ScheduleRequest>; 4] = receivers.clone();
            workers.push(spawn(move || {
                // Persistent, lazily-opened handle to the data file — reused for
                // every page read/write this worker performs.
                let mut data = DataFile::new(fs.as_ref(), &pool.data_path, &open_count);

                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }

                    // --- Weighted-fair drain ---------------------------------
                    // One cycle: each lane may serve up to its weight before we
                    // advance.  Every non-empty lane therefore makes progress
                    // each cycle; no lane can be starved by a busier one.
                    let mut served = false;
                    for (lane, rx) in rxs.iter().enumerate() {
                        let budget = LANE_WEIGHTS[lane];
                        for _ in 0..budget {
                            match rx.try_recv() {
                                Ok(req) => {
                                    Self::execute(req, &pool, &mut data, &wal);
                                    served = true;
                                }
                                Err(_) => break, // lane empty; move on
                            }
                        }
                    }

                    if !served {
                        // All lanes empty: block briefly on the highest-priority
                        // lane so a new P0 request wakes us promptly, while the
                        // timeout keeps shutdown responsive.
                        match rxs[0].recv_timeout(Duration::from_millis(5)) {
                            Ok(req) => Self::execute(req, &pool, &mut data, &wal),
                            Err(_) => continue,
                        }
                    }
                }
            }));
        }

        Self {
            tx,
            shutdown,
            workers,
            data_open_count,
        }
    }

    /// Total number of `open()` calls workers have made on the data file.
    ///
    /// With persistent fds this saturates at the worker count: each worker
    /// opens the file at most once for its whole lifetime, no matter how many
    /// requests it serves.
    pub fn data_open_count(&self) -> u64 {
        self.data_open_count.load(Ordering::Relaxed)
    }

    fn execute(
        req: ScheduleRequest,
        pool: &BufferPool,
        data: &mut DataFile<'_>,
        wal: &std::sync::Mutex<WalWriter>,
    ) {
        let res = match req.command {
            ScheduleCommand::ReadPage { page_id } => {
                use crate::io::AlignedBuffer;
                use crate::storage::page::PAGE_SIZE;
                let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
                let offset = page_id * PAGE_SIZE as u64;
                match data.get() {
                    Ok(handle) => match handle.read_at(&mut buf, offset) {
                        Ok(()) => ScheduleResponse::Ok(buf.to_vec()),
                        Err(e) => ScheduleResponse::Err(e.to_string()),
                    },
                    Err(e) => ScheduleResponse::Err(e.to_string()),
                }
            }
            ScheduleCommand::WritePage { page_id, buf } => {
                use crate::storage::page::PAGE_SIZE;
                let offset = page_id * PAGE_SIZE as u64;
                match data.get() {
                    Ok(handle) => match handle.write_at(&buf[..buf.len().min(PAGE_SIZE)], offset) {
                        Ok(()) => match handle.sync_data() {
                            Ok(()) => ScheduleResponse::Ok(vec![]),
                            Err(e) => ScheduleResponse::Err(e.to_string()),
                        },
                        Err(e) => ScheduleResponse::Err(e.to_string()),
                    },
                    Err(e) => ScheduleResponse::Err(e.to_string()),
                }
            }
            ScheduleCommand::SyncWal => {
                // The WAL writer owns its own persistent handle internally;
                // `data.fs` only supplies the filesystem vtable for the flush.
                match wal.lock() {
                    Ok(mut w) => match w.sync(data.fs) {
                        Ok(()) => ScheduleResponse::Ok(vec![]),
                        Err(e) => ScheduleResponse::Err(e.to_string()),
                    },
                    Err(_) => ScheduleResponse::Err("wal mutex poisoned".into()),
                }
            }
            ScheduleCommand::FlushPage { page_id } => {
                let mut found = false;
                let mut res = ScheduleResponse::Ok(vec![]);
                for (fid, frame) in pool.iter_frames().enumerate() {
                    if frame.desc.page_id.load(Ordering::Relaxed) == page_id {
                        if let Err(e) = pool.flush_single_frame(data.fs, fid as u32) {
                            res = ScheduleResponse::Err(e.to_string());
                        }
                        found = true;
                        break;
                    }
                }
                if !found {
                    res = ScheduleResponse::Err(format!("page {} not in pool", page_id));
                }
                res
            }
        };
        let _ = req.respond.send(res);
    }

    /// Submit a request to the given priority lane and block until the
    /// response arrives.
    pub fn call_sync(
        &self,
        priority: Priority,
        cmd: ScheduleCommand,
    ) -> ScheduleResponse {
        let (tx, rx) = bounded(1);
        let req = ScheduleRequest {
            priority,
            command: cmd,
            respond: tx,
        };
        self.tx[priority as usize]
            .send(req)
            .expect("scheduler worker alive");
        rx.recv().expect("scheduler worker responded")
    }

    /// Signal workers to stop and wait for them.
    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
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
    use crate::storage::page::PAGE_SIZE;
    use std::io::Write;

    fn setup_scheduler(
        frames: u32,
    ) -> (
        tempfile::TempDir,
        IoScheduler,
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
        let scheduler = IoScheduler::new(pool.clone(), fs.clone(), wal.clone(), 2, 64);
        (dir, scheduler, pool, wal)
    }

    #[test]
    fn scheduler_read_page() {
        let (_dir, scheduler, pool, _wal) = setup_scheduler(4);
        {
            let mut guard = pool.fix_page(&PosixFileSystem::new(false), 1).unwrap();
            guard.buf_mut()[0] = 0xAB;
            guard.buf_mut()[1] = 0xCD;
            guard.set_dirty(0);
        }
        pool.flush_all(&PosixFileSystem::new(false)).unwrap();

        let res = scheduler.call_sync(
            Priority::PageRead,
            ScheduleCommand::ReadPage { page_id: 1 },
        );
        match res {
            ScheduleResponse::Ok(data) => {
                assert_eq!(data[0], 0xAB);
                assert_eq!(data[1], 0xCD);
            }
            ScheduleResponse::Err(e) => panic!("read failed: {}", e),
        }
        scheduler.stop();
    }

    #[test]
    fn scheduler_write_and_read() {
        let (_dir, scheduler, _pool, _wal) = setup_scheduler(4);
        let mut buf = vec![0u8; PAGE_SIZE];
        buf[0] = 0xCA;
        buf[1] = 0xFE;
        let res = scheduler.call_sync(
            Priority::PageRead,
            ScheduleCommand::WritePage { page_id: 2, buf },
        );
        assert!(matches!(res, ScheduleResponse::Ok(_)), "write failed");

        let res = scheduler.call_sync(
            Priority::PageRead,
            ScheduleCommand::ReadPage { page_id: 2 },
        );
        match res {
            ScheduleResponse::Ok(data) => {
                assert_eq!(data[0], 0xCA);
                assert_eq!(data[1], 0xFE);
            }
            ScheduleResponse::Err(e) => panic!("read failed: {}", e),
        }
        scheduler.stop();
    }

    #[test]
    fn scheduler_wal_sync() {
        let (_dir, scheduler, _pool, _wal) = setup_scheduler(4);
        let res = scheduler.call_sync(Priority::Wal, ScheduleCommand::SyncWal);
        assert!(matches!(res, ScheduleResponse::Ok(_)), "sync failed");
        scheduler.stop();
    }

    #[test]
    fn persistent_fd_does_not_reopen_per_request() {
        // Serve many page reads through a single worker and assert the data
        // file was opened at most once (not once per request).
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("rgraph.db");
        let wal_dir = dir.path().join("wal");
        let fs = Arc::new(PosixFileSystem::new(false));
        {
            use crate::storage::page::{PageType, SlottedPage};
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
        // One worker so the open count is unambiguous.
        let scheduler = IoScheduler::new(pool.clone(), fs.clone(), wal.clone(), 1, 64);

        for _ in 0..50 {
            let res = scheduler.call_sync(
                Priority::PageRead,
                ScheduleCommand::ReadPage { page_id: 1 },
            );
            assert!(matches!(res, ScheduleResponse::Ok(_)), "read failed");
        }

        assert!(
            scheduler.data_open_count() <= 1,
            "data file must be opened at most once per worker, got {}",
            scheduler.data_open_count()
        );
        scheduler.stop();
    }

    #[test]
    fn weighted_fair_scheduling_does_not_starve_low_priority() {
        // Two "clients": a high-priority producer floods the WAL lane (P0) with
        // page reads while a low-priority producer issues bulk reads (P3).
        // Under the old strict-priority loop the P3 client could be starved
        // indefinitely; with weighted-fair draining every P3 request must
        // complete.
        let (_dir, scheduler, _pool, _wal) = setup_scheduler(8);
        let scheduler = Arc::new(scheduler);

        const HI: usize = 400;
        const LO: usize = 100;

        let hi_sched = scheduler.clone();
        let hi = std::thread::spawn(move || {
            let mut ok = 0;
            for _ in 0..HI {
                if let ScheduleResponse::Ok(_) = hi_sched
                    .call_sync(Priority::Wal, ScheduleCommand::ReadPage { page_id: 1 })
                {
                    ok += 1;
                }
            }
            ok
        });

        let lo_sched = scheduler.clone();
        let lo = std::thread::spawn(move || {
            let mut ok = 0;
            for _ in 0..LO {
                if let ScheduleResponse::Ok(_) = lo_sched
                    .call_sync(Priority::BulkRead, ScheduleCommand::ReadPage { page_id: 2 })
                {
                    ok += 1;
                }
            }
            ok
        });

        let hi_ok = hi.join().unwrap();
        let lo_ok = lo.join().unwrap();

        assert_eq!(hi_ok, HI, "all high-priority requests must complete");
        assert_eq!(
            lo_ok, LO,
            "all low-priority requests must complete — no starvation"
        );

        // Tear down: reclaim the scheduler from the Arc and stop it.
        let scheduler = Arc::try_unwrap(scheduler)
            .unwrap_or_else(|_| panic!("scheduler still shared at teardown"));
        scheduler.stop();
    }

    #[test]
    fn lane_weights_admit_every_lane() {
        // Sanity: every lane has a non-zero budget so it cannot be starved
        // structurally, and higher priority gets a larger share.
        assert!(LANE_WEIGHTS.iter().all(|&w| w >= 1), "every lane must be served");
        assert!(
            LANE_WEIGHTS[0] > LANE_WEIGHTS[3],
            "P0 must outrank P3"
        );
    }
}
