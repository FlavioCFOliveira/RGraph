use crate::buffer::pool::BufferPool;
use crate::io::FileSystem;
use crate::wal::writer::WalWriter;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{sleep, spawn, JoinHandle};
use std::time::{Duration, Instant};

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

/// A production multi-queue I/O scheduler with strict priority lanes.
///
/// P0 uses a fast-lane bounded queue.  P3 yields after every 32 requests
/// or 1 ms to prevent head-of-line blocking.
pub struct IoScheduler {
    /// Senders for each priority lane (shared by all producers).
    pub tx: [Sender<ScheduleRequest>; 4],
    /// Signal to stop.
    shutdown: Arc<AtomicBool>,
    /// Worker handles.
    workers: Vec<JoinHandle<()>>,
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
        let mut workers = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let shutdown = shutdown.clone();
            let pool = pool.clone();
            let fs = fs.clone();
            let wal = wal.clone();
            let rxs: [Receiver<ScheduleRequest>; 4] = receivers.clone();
            workers.push(spawn(move || {
                let mut p3_count: u32 = 0;
                let mut p3_start = Instant::now();
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }

                    // Strict priority: try P0 .. P3 in order.
                    let mut found = false;
                    for (lane, rx) in rxs.iter().enumerate() {
                        match rx.try_recv() {
                            Ok(req) => {
                                // P3 yield logic.
                                if lane == 3 {
                                    p3_count += 1;
                                    if p3_count >= 32
                                        || p3_start.elapsed() >= Duration::from_millis(1)
                                    {
                                        p3_count = 0;
                                        p3_start = Instant::now();
                                        sleep(Duration::from_micros(10));
                                    }
                                }
                                Self::execute(req, &pool, fs.as_ref(), &wal);
                                found = true;
                                break; // re-scan from P0 after each request
                            }
                            Err(_) => continue,
                        }
                    }

                    if !found {
                        // Nothing available on any lane; block on P0
                        // (or any lane) with a short timeout so we can
                        // check shutdown periodically.
                        match rxs[0].recv_timeout(Duration::from_millis(5)) {
                            Ok(req) => Self::execute(req, &pool, fs.as_ref(), &wal),
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
        }
    }

    fn execute(
        req: ScheduleRequest,
        pool: &BufferPool,
        fs: &dyn FileSystem,
        wal: &std::sync::Mutex<WalWriter>,
    ) {
        let res = match req.command {
            ScheduleCommand::ReadPage { page_id } => {
                use crate::io::AlignedBuffer;
                use crate::storage::page::PAGE_SIZE;
                let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
                let offset = page_id * PAGE_SIZE as u64;
                match fs.open(&pool.data_path, false) {
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
                match fs.open(&pool.data_path, false) {
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
                match wal.lock() {
                    Ok(mut w) => match w.sync(fs) {
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
                        if let Err(e) = pool.flush_single_frame(fs, fid as u32) {
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
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("rgraph.db");
        let wal_dir = dir.path().join("wal");
        let fs = Arc::new(PosixFileSystem::new(false));

        let mut f = std::fs::File::create(&data_path).unwrap();
        f.set_len((frames as u64).max(64) * PAGE_SIZE as u64).unwrap();
        f.flush().unwrap();
        drop(f);

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
}
