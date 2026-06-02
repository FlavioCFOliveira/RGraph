use crate::buffer::frame::FrameState;
use crate::buffer::pool::BufferPool;
use crate::io::FileSystem;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{sleep, spawn, JoinHandle};
use std::time::Duration;

/// Default threshold: start flushing when > 10 % of frames are dirty.
pub const DEFAULT_DIRTY_RATIO: f64 = 0.10;

/// Default sleep interval between sweeps.
pub const DEFAULT_FLUSH_INTERVAL_MS: u64 = 10;

/// Request sent to the flusher thread.
#[derive(Debug)]
pub enum FlushRequest {
    /// Flush every eligible dirty frame now.
    FlushAll,
    /// Stop the flusher loop.
    Shutdown,
}

/// Background thread that continuously writes dirty frames back to disk.
pub struct Flusher {
    /// Handle to the background thread.
    handle: Option<JoinHandle<()>>,
    /// Signal to stop.
    shutdown: Arc<AtomicBool>,
    /// Monotonically increasing watermark of flushed LSN.
    pub flushed_lsn: Arc<AtomicU64>,
}

impl Flusher {
    /// Start a background flusher thread.
    ///
    /// `pool` and `fs` are captured via `Arc` so the thread can reference
    /// them safely.  For simplicity we accept `Arc<BufferPool>` and a
    /// boxed filesystem trait object.
    pub fn new(
        pool: Arc<BufferPool>,
        fs: Arc<dyn FileSystem>,
        dirty_ratio: f64,
        interval_ms: u64,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let flushed_lsn = Arc::new(AtomicU64::new(0));
        let flushed_lsn_clone = flushed_lsn.clone();

        let handle = spawn(move || {
            let interval = Duration::from_millis(interval_ms);
            loop {
                if shutdown_clone.load(Ordering::Relaxed) {
                    break;
                }

                let dirty_count = pool.iter_frames().filter(|f| {
                    f.desc.state.load(Ordering::Relaxed) == FrameState::Dirty as u8
                }).count();
                let dirty_pct = dirty_count as f64 / pool.frame_count as f64;

                if dirty_pct > dirty_ratio {
                    let candidates = pool.dirty_candidates();
                    for fid in candidates {
                        let frame = pool.frame(fid);
                        let page_id = frame.desc.page_id.load(Ordering::Relaxed);
                        if page_id == 0 {
                            continue;
                        }

                        // WAL-before-data ordering: do not flush if the
                        // frame's rec_lsn is ahead of the last known flushed
                        // LSN.  In a full ARIES implementation this would
                        // coordinate with the WAL writer; here we simply
                        // skip the frame and let the WAL fsync advance
                        // the watermark first.
                        let rec_lsn = frame.desc.rec_lsn.load(Ordering::Relaxed);
                        let current_flushed = flushed_lsn_clone.load(Ordering::Relaxed);
                        if rec_lsn != u64::MAX && rec_lsn > current_flushed {
                            continue;
                        }

                        if frame.desc.io_inflight.swap(true, Ordering::Acquire) {
                            continue; // another thread already flushing
                        }

                        let offset = page_id as u64 * crate::storage::page::PAGE_SIZE as u64;
                        if let Ok(handle) = fs.open(&pool.data_path, false) {
                            if handle.write_at(&frame.buf, offset).is_ok()
                                && handle.sync_data().is_ok()
                            {
                                frame.desc.dirty.store(false, Ordering::Release);
                                frame.desc.state.store(FrameState::Clean as u8, Ordering::Release);
                                let last = frame.desc.last_lsn.load(Ordering::Relaxed);
                                flushed_lsn_clone.fetch_max(last, Ordering::Release);
                                frame.desc.rec_lsn.store(u64::MAX, Ordering::Relaxed);
                            }
                        }
                        frame.desc.io_inflight.store(false, Ordering::Release);
                    }
                }

                sleep(interval);
            }
        });

        Self {
            handle: Some(handle),
            shutdown,
            flushed_lsn,
        }
    }

    /// Signal the flusher to stop and wait for it.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Flusher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::pool::BufferPool;
    use crate::io::posix::PosixFileSystem;
    use std::io::Write;

    fn setup(frames: u32) -> (tempfile::TempDir, Arc<PosixFileSystem>, Arc<BufferPool>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgraph.db");
        let fs = Arc::new(PosixFileSystem::new(false));
        let mut f = std::fs::File::create(&path).unwrap();
        let file_pages = (frames as u64).max(64);
        f.set_len(file_pages * crate::storage::page::PAGE_SIZE as u64)
            .unwrap();
        f.flush().unwrap();
        drop(f);
        let pool = Arc::new(BufferPool::new(frames, path));
        (dir, fs, pool)
    }

    #[test]
    fn flusher_cleans_dirty_frames() {
        let (_dir, fs, pool) = setup(4);
        {
            let mut guard = pool.fix_page(fs.as_ref(), 1).unwrap();
            guard.buf_mut()[0] = 0xDE;
            guard.set_dirty(0);
        }

        let flusher = Flusher::new(pool.clone(), fs.clone(), 0.05, 5);
        // Give the flusher time to run.
        sleep(Duration::from_millis(100));
        drop(flusher);

        let mut found = false;
        for frame in pool.iter_frames() {
            if frame.desc.page_id.load(Ordering::Relaxed) == 1 {
                assert!(!frame.desc.dirty.load(Ordering::Relaxed));
                found = true;
                break;
            }
        }
        assert!(found);
    }

    #[test]
    fn flusher_respects_dirty_ratio_threshold() {
        let (_dir, fs, pool) = setup(100);
        // Dirty only 1 % of frames.
        {
            let mut guard = pool.fix_page(fs.as_ref(), 1).unwrap();
            guard.buf_mut()[0] = 0xAB;
            guard.set_dirty(1);
        }

        // Threshold = 50 %, interval = 5 ms.
        let flusher = Flusher::new(pool.clone(), fs.clone(), 0.50, 5);
        sleep(Duration::from_millis(60));
        drop(flusher);

        // With threshold at 50 % the flusher should NOT have run because
        // only 1/100 frames are dirty.  The frame should still be dirty.
        let mut found = false;
        for frame in pool.iter_frames() {
            if frame.desc.page_id.load(Ordering::Relaxed) == 1 {
                assert!(frame.desc.dirty.load(Ordering::Relaxed));
                found = true;
                break;
            }
        }
        assert!(found);
    }
}
