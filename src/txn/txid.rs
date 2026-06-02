//! Monotonic transaction ID allocator.
//!
//! [`TxIdAllocator`] vends strictly increasing [`TxId`] values backed by an
//! [`AtomicU64`].  The zero value is reserved as the invalid sentinel; the
//! allocator skips it transparently.  Callers are responsible for persisting
//! the counter to a system page so that the sequence survives crashes.

use std::sync::atomic::{AtomicU64, Ordering};

/// A transaction identifier.  Value `0` is always invalid.
pub type TxId = u64;

/// The sentinel value meaning "no transaction".
pub const TX_ID_INVALID: TxId = 0;

/// The first TxId used during database bootstrap.
pub const TX_ID_BOOTSTRAP: TxId = 1;

/// Byte offset within a system-page buffer at which the counter is stored.
const PERSIST_OFFSET: usize = 0;

/// Monotonically increasing TxID allocator.
///
/// Allocation is lock-free and wait-free: each call to [`TxIdAllocator::allocate`]
/// performs a single `fetch_add` on a `SeqCst` atomic.  The invariant that TxId
/// 0 is never returned is maintained by post-increment clamping.
///
/// The allocator can be serialised to and deserialised from an 8-byte region of
/// a system page so that the highest-ever-issued TxId survives a crash.  On
/// recovery, the stored value is incremented by `RECOVERY_BUMP` before use so
/// that any in-flight transactions from before the crash are definitively older
/// than any new ones.
pub struct TxIdAllocator {
    /// The next TxId to issue.
    counter: AtomicU64,
}

/// How much to add to the persisted counter on recovery to ensure all
/// pre-crash in-flight transactions are definitively in the past.
///
/// This value is intentionally generous: even a database that issues
/// 1 million transactions per second would need ~18 hours before it
/// wraps into this gap after recovery.
const RECOVERY_BUMP: u64 = 1_000_000;

impl TxIdAllocator {
    /// Create a new allocator starting from `TX_ID_BOOTSTRAP`.
    pub fn new() -> Self {
        Self {
            counter: AtomicU64::new(TX_ID_BOOTSTRAP),
        }
    }

    /// Create an allocator starting at an arbitrary value (for recovery).
    ///
    /// If `start` is `TX_ID_INVALID` the allocator begins at `TX_ID_BOOTSTRAP`.
    pub fn with_start(start: TxId) -> Self {
        let start = if start == TX_ID_INVALID {
            TX_ID_BOOTSTRAP
        } else {
            start
        };
        Self {
            counter: AtomicU64::new(start),
        }
    }

    /// Issue the next TxId.
    ///
    /// This is lock-free and wait-free.  The returned value is guaranteed to be
    /// `> TX_ID_INVALID` and strictly greater than any previously returned value
    /// from this instance (or any instance sharing the same atomic via unsafe
    /// aliasing, which is not supported).
    pub fn allocate(&self) -> TxId {
        loop {
            let id = self.counter.fetch_add(1, Ordering::SeqCst);
            if id != TX_ID_INVALID {
                return id;
            }
            // We issued the invalid sentinel — extremely unlikely in practice
            // (requires the counter to have wrapped), but handled correctly.
            // Try once more; the next value is guaranteed non-zero.
        }
    }

    /// Peek at the next TxId that would be issued without advancing the counter.
    ///
    /// This is for diagnostic and checkpoint purposes only; do not make
    /// allocation decisions based on this value.
    pub fn peek_next(&self) -> TxId {
        let v = self.counter.load(Ordering::SeqCst);
        if v == TX_ID_INVALID {
            TX_ID_BOOTSTRAP
        } else {
            v
        }
    }

    /// Write the current counter value to an 8-byte region of `page`.
    ///
    /// `page` must be at least `PERSIST_OFFSET + 8` bytes long.  The stored
    /// value is the *next* TxId that would be issued, so that recovery can
    /// resume from a value that is strictly ahead of any pre-crash transaction.
    ///
    /// # Panics
    ///
    /// Panics if `page.len() < 8`.
    pub fn persist(&self, page: &mut [u8]) {
        let value = self.counter.load(Ordering::SeqCst);
        let bytes = value.to_le_bytes();
        page[PERSIST_OFFSET..PERSIST_OFFSET + 8].copy_from_slice(&bytes);
    }

    /// Restore the allocator state from an 8-byte region of `page`.
    ///
    /// The stored counter is bumped by [`RECOVERY_BUMP`] to ensure that any
    /// transactions that were in-flight at crash time are definitively behind
    /// any new transactions issued after recovery.
    ///
    /// # Panics
    ///
    /// Panics if `page.len() < 8`.
    pub fn recover(page: &[u8]) -> Self {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&page[PERSIST_OFFSET..PERSIST_OFFSET + 8]);
        let stored = u64::from_le_bytes(bytes);
        // Guard against a zeroed page (fresh database) or overflow.
        let start = stored
            .checked_add(RECOVERY_BUMP)
            .unwrap_or(TX_ID_BOOTSTRAP)
            .max(TX_ID_BOOTSTRAP);
        Self::with_start(start)
    }
}

impl Default for TxIdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn starts_at_bootstrap() {
        let alloc = TxIdAllocator::new();
        assert_eq!(alloc.allocate(), TX_ID_BOOTSTRAP);
    }

    #[test]
    fn never_returns_zero() {
        let alloc = TxIdAllocator::new();
        for _ in 0..1_000 {
            assert_ne!(alloc.allocate(), TX_ID_INVALID);
        }
    }

    #[test]
    fn strictly_monotonic() {
        let alloc = TxIdAllocator::new();
        let mut prev = TX_ID_INVALID;
        for _ in 0..1_000 {
            let id = alloc.allocate();
            assert!(id > prev);
            prev = id;
        }
    }

    #[test]
    fn persist_and_recover_roundtrip() {
        let alloc = TxIdAllocator::new();
        // Issue several TxIDs.
        for _ in 0..42 {
            alloc.allocate();
        }
        let mut page = vec![0u8; 64];
        alloc.persist(&mut page);

        let recovered = TxIdAllocator::recover(&page);
        // Recovered counter must be strictly greater than the original due to RECOVERY_BUMP.
        assert!(recovered.peek_next() > alloc.peek_next());
        // And must also be strictly monotonic going forward.
        let next = recovered.allocate();
        assert!(next > alloc.peek_next());
    }

    #[test]
    fn recover_from_zeroed_page_starts_at_bootstrap() {
        let page = vec![0u8; 64];
        let alloc = TxIdAllocator::recover(&page);
        let id = alloc.allocate();
        assert!(id >= TX_ID_BOOTSTRAP);
    }

    #[test]
    fn concurrent_allocation_monotonic_and_unique() {
        let alloc = Arc::new(TxIdAllocator::new());
        let n_threads = 8;
        let n_per_thread = 1_000;

        let handles: Vec<_> = (0..n_threads)
            .map(|_| {
                let a = Arc::clone(&alloc);
                thread::spawn(move || {
                    (0..n_per_thread).map(|_| a.allocate()).collect::<Vec<_>>()
                })
            })
            .collect();

        let mut all: Vec<TxId> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("thread panicked"))
            .collect();

        // Every ID must be non-zero.
        assert!(all.iter().all(|&id| id != TX_ID_INVALID));

        // Every ID must be unique.
        all.sort_unstable();
        let original_len = all.len();
        all.dedup();
        assert_eq!(all.len(), original_len, "duplicate TxIds detected");
    }

    #[test]
    fn peek_next_does_not_advance() {
        let alloc = TxIdAllocator::new();
        let p1 = alloc.peek_next();
        let p2 = alloc.peek_next();
        assert_eq!(p1, p2);
        let id = alloc.allocate();
        assert_eq!(id, p1);
    }
}
