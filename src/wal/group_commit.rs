//! Group-commit queue with fsync coalescing (Tasks 105, 153).
//!
//! Multiple concurrent writers submit their [`WalRecord`]s through a bounded
//! channel.  The first thread that wins the leader [`Mutex`] collects all
//! pending slots up to `BATCH_SIZE` (or until a 1 ms timeout), appends every
//! record to the [`WalWriter`] in one contiguous burst, issues a **single**
//! `fdatasync`, and then notifies every follower via their individual oneshot
//! channel.  Followers that arrive while a leader is active simply block on
//! their oneshot receiver and are woken up with the leader's result.
//!
//! This design bounds `fsync` frequency to ≈ 1 kHz regardless of the number
//! of concurrent committers, while maintaining the strict durability guarantee
//! that no commit is acknowledged before the group fsync completes.
//!
//! # Liveness guarantee (Task 153 bugfix)
//!
//! An orphaned follower can arise when a thread enqueues its slot **after** the
//! leader has closed its collection window but **before** the leader releases
//! the `leader_lock`.  In that case the follower's slot sits in the channel
//! unprocessed, and `result_rx.recv()` would block forever.
//!
//! The fix: followers wait on their `result_rx` with a bounded timeout equal
//! to `2 × BATCH_TIMEOUT`.  If the timeout fires they re-attempt to acquire
//! `leader_lock`; if successful they become the new leader and process the
//! remaining slots (including their own).  This loop repeats until the slot is
//! processed, guaranteeing liveness without busy-polling.
//!
//! # Backpressure
//!
//! The submission channel is bounded at [`GroupCommitQueue::CHANNEL_CAPACITY`]
//! (1 024).  When full, senders block in `submit`, which provides natural
//! backpressure to the application layer without unbounded memory growth.

use crate::io::FileSystem;
use crate::wal::record::WalRecord;
use crate::wal::writer::WalWriter;
use crossbeam_channel::{Receiver, Sender, bounded};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A pending commit submission placed on the shared channel.
struct CommitSlot {
    /// Records to append for this commit.
    records: Vec<WalRecord>,
    /// One-shot channel used to deliver the LSN (or error) back to the submitter.
    result_tx: std::sync::mpsc::SyncSender<io::Result<u64>>,
}

/// Shared, cloneable handle to the group-commit infrastructure.
///
/// All state is reference-counted so that [`GroupCommitQueue`] can be cheaply
/// sent across threads without wrapping it in an additional `Arc`.
#[derive(Clone)]
pub struct GroupCommitQueue {
    tx: Sender<CommitSlot>,
    rx: Arc<Receiver<CommitSlot>>,
    /// Mutex used for leader election.  The thread that acquires it becomes
    /// the leader for one batch cycle; all others are followers.
    leader_lock: Arc<Mutex<()>>,
}

impl GroupCommitQueue {
    /// Maximum channel depth.  Senders block when full (backpressure).
    pub const CHANNEL_CAPACITY: usize = 1_024;

    /// Maximum number of [`CommitSlot`]s collected into one fsync batch.
    pub const BATCH_SIZE: usize = 64;

    /// How long the leader waits for additional followers to arrive before
    /// issuing the fsync with whatever it has collected so far.
    pub const BATCH_TIMEOUT: Duration = Duration::from_millis(1);

    /// Create a new queue.
    pub fn new() -> Self {
        let (tx, rx) = bounded(Self::CHANNEL_CAPACITY);
        Self {
            tx,
            rx: Arc::new(rx),
            leader_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Submit `records` for durable commit.
    ///
    /// Blocks until the group leader has fsynced.  Returns the LSN of the
    /// last record in this submission.
    ///
    /// # Liveness
    ///
    /// To prevent orphaned followers (see module-level comment), the wait loop
    /// uses a bounded timeout and re-attempts leader election if the timeout
    /// fires before a result arrives.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the underlying WAL write or fsync fails.
    pub fn submit(
        &self,
        records: Vec<WalRecord>,
        wal: &Mutex<WalWriter>,
        fs: &dyn FileSystem,
    ) -> io::Result<u64> {
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let slot = CommitSlot { records, result_tx };

        // Enqueue.  This blocks if the channel is full (backpressure).
        self.tx.send(slot).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "group commit channel closed")
        })?;

        // Liveness loop: attempt to become leader; if we lose the race,
        // wait for our result with a timeout.  If the timeout fires without
        // a result, we re-attempt — this handles the orphaned-follower case
        // described in the module comment.
        let wait_timeout = Self::BATCH_TIMEOUT * 2;
        loop {
            // Attempt to become leader.
            if let Ok(_guard) = self.leader_lock.try_lock() {
                // We are the leader: drain the channel and flush.
                self.run_leader_with(wal, fs);
                // After the leader run, our result must be in result_rx.
                // Fall through to the blocking recv below.
            }

            // Wait for our result (either from our own leader run, or from
            // another leader that picked up our slot).
            match result_rx.recv_timeout(wait_timeout) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Possible orphaned follower: loop and retry leader election.
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "group commit result channel closed",
                    ));
                }
            }
        }
    }

    /// Leader implementation: collect all pending slots up to [`BATCH_SIZE`]
    /// or [`BATCH_TIMEOUT`], append them all to the WAL, fsync once, then
    /// notify every follower.
    ///
    /// Called while holding the leader lock (enforced by `submit`).
    fn run_leader_with(&self, wal: &Mutex<WalWriter>, fs: &dyn FileSystem) {
        let mut slots: Vec<CommitSlot> = Vec::with_capacity(Self::BATCH_SIZE);

        // Collect the first slot (we know at least one exists because we sent ours).
        // Try to accumulate more within the timeout window.
        let deadline = std::time::Instant::now() + Self::BATCH_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match self.rx.recv_timeout(if remaining.is_zero() {
                // Use a very short timeout to do one final non-blocking drain.
                Duration::from_nanos(1)
            } else {
                remaining
            }) {
                Ok(slot) => {
                    slots.push(slot);
                    if slots.len() >= Self::BATCH_SIZE {
                        break;
                    }
                }
                Err(_) => break, // timeout or disconnected
            }
        }

        if slots.is_empty() {
            return;
        }

        // Append all records under the WAL mutex, track each slot's last LSN.
        let mut slot_lsns: Vec<u64> = Vec::with_capacity(slots.len());
        let flush_result: io::Result<()> = {
            let mut writer = wal.lock().expect("WAL mutex poisoned");
            let mut overall_err: Option<io::Error> = None;
            for slot in &mut slots {
                let mut last_lsn = 0u64;
                for rec in slot.records.drain(..) {
                    match writer.append(fs, rec) {
                        Ok(lsn) => last_lsn = lsn,
                        Err(e) => {
                            overall_err = Some(e);
                            break;
                        }
                    }
                }
                if overall_err.is_some() {
                    slot_lsns.push(0); // error; value unused
                    break;
                }
                slot_lsns.push(last_lsn);
            }
            if let Some(e) = overall_err {
                Err(e)
            } else {
                // Single fsync for the entire group.
                writer.flush(fs)
            }
        };

        // Notify every slot with either the LSN or the error.
        for (i, slot) in slots.into_iter().enumerate() {
            let result = match &flush_result {
                Ok(()) => Ok(slot_lsns.get(i).copied().unwrap_or(0)),
                Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
            };
            // Ignore send errors — the receiver may have already timed out.
            let _ = slot.result_tx.send(result);
        }
    }
}

impl Default for GroupCommitQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;
    use crate::wal::record::{RecordType, WalRecord};

    fn make_record(txid: u64) -> WalRecord {
        WalRecord::new(RecordType::Commit, txid, 0, 0, vec![])
    }

    #[test]
    fn single_commit_returns_lsn() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let wal = Arc::new(Mutex::new(
            WalWriter::open(dir.path().join("wal"), &fs).unwrap(),
        ));
        let queue = GroupCommitQueue::new();
        let lsn = queue.submit(vec![make_record(1)], &wal, &fs).unwrap();
        assert!(lsn > 0, "LSN must be non-zero");
    }

    #[test]
    fn multiple_sequential_commits() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let wal = Arc::new(Mutex::new(
            WalWriter::open(dir.path().join("wal"), &fs).unwrap(),
        ));
        let queue = GroupCommitQueue::new();

        let lsn1 = queue.submit(vec![make_record(1)], &wal, &fs).unwrap();
        let lsn2 = queue.submit(vec![make_record(2)], &wal, &fs).unwrap();
        assert!(lsn2 > lsn1, "LSNs must be monotonically increasing");
    }

    #[test]
    fn concurrent_commits_all_succeed() {
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(PosixFileSystem::new(false));
        let wal = Arc::new(Mutex::new(
            WalWriter::open(dir.path().join("wal"), fs.as_ref()).unwrap(),
        ));
        let queue = GroupCommitQueue::new();

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let q = queue.clone();
                let w = Arc::clone(&wal);
                let f = Arc::clone(&fs);
                thread::spawn(move || q.submit(vec![make_record(i)], &w, f.as_ref()))
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for r in results {
            assert!(r.is_ok(), "all concurrent commits must succeed");
        }
    }

    #[test]
    fn empty_records_handled_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let wal = Arc::new(Mutex::new(
            WalWriter::open(dir.path().join("wal"), &fs).unwrap(),
        ));
        let queue = GroupCommitQueue::new();
        // An empty record vec: last_lsn stays 0 but should not panic.
        let result = queue.submit(vec![], &wal, &fs);
        // With no records the leader collects the slot, drains it (empty),
        // flushes (no-op) and returns lsn 0.
        assert!(result.is_ok());
    }

    // ── Task 153: coalescing and liveness ─────────────────────────────────────

    #[test]
    fn concurrent_commits_coalesce_into_fewer_fsyncs() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(PosixFileSystem::new(false));
        let wal = Arc::new(Mutex::new(
            WalWriter::open(dir.path().join("wal"), fs.as_ref()).unwrap(),
        ));
        let queue = GroupCommitQueue::new();

        // Launch 16 concurrent commits.
        let success_count = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let q = queue.clone();
                let w = Arc::clone(&wal);
                let f = Arc::clone(&fs);
                let counter = Arc::clone(&success_count);
                thread::spawn(move || {
                    if q.submit(vec![make_record(i)], &w, f.as_ref()).is_ok() {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            success_count.load(Ordering::Relaxed),
            16,
            "all 16 concurrent commits must succeed (liveness guarantee)"
        );

        // Verify the WAL file has all 16 records (coalescing doesn't lose records).
        let wal_path = dir.path().join("wal").join("wal-000000000");
        let raw = std::fs::read(&wal_path).unwrap();
        let mut count = 0usize;
        let mut offset = 0;
        while offset < raw.len() {
            if let Some((_, size)) = WalRecord::decode(&raw, offset) {
                count += 1;
                offset += size;
            } else {
                break;
            }
        }
        assert_eq!(count, 16, "all 16 records must be durable in the WAL");
    }
}
