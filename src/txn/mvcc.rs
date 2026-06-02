//! MVCC tuple version headers.
//!
//! [`TupleHeader`] is a 16-byte, `#[repr(C)]` structure that prefixes every
//! versioned record on a slotted page.  It carries the transaction IDs that
//! created and (optionally) deleted this version, a command-id for savepoint
//! support, an infomask bitmask for cached commit/abort status, and a
//! page-local offset to the next version in the chain.
//!
//! The design follows PostgreSQL's HeapTupleHeaderData approach, adapted for
//! RGraph's fixed-width record layout.
//!
//! # TxId truncation
//!
//! The on-disk tuple header stores `xmin` and `xmax` as `u32` (matching
//! PostgreSQL's `TransactionId` width).  The full 64-bit [`TxId`] used in
//! memory fits in 32 bits for practical database sizes (4 billion transactions
//! before wrap-around, handled by the vacuum/epoch-advance mechanism).  The
//! in-memory transaction system uses `u64` for simplicity; the tuple header
//! truncates on persist and zero-extends on load.

use crate::txn::txid::{TxId, TX_ID_INVALID};
use std::mem::size_of;

/// Sentinel value for `xmin`/`xmax` stored in a tuple header (32-bit form).
pub const TUPLE_TXID_INVALID: u32 = 0;

/// Infomask bit-flags stored in [`TupleHeader::infomask`].
///
/// These flags cache the commit/abort status of the creating and deleting
/// transactions so that visibility checks can avoid repeated lookups in the
/// global transaction state.
pub mod infomask {
    /// The transaction that created this version (`xmin`) has committed.
    pub const XMIN_COMMITTED: u16 = 0x0001;
    /// The transaction that created this version (`xmin`) has aborted.
    pub const XMIN_ABORTED: u16 = 0x0002;
    /// The transaction that deleted this version (`xmax`) has committed.
    pub const XMAX_COMMITTED: u16 = 0x0004;
    /// The transaction that deleted this version (`xmax`) has aborted.
    pub const XMAX_ABORTED: u16 = 0x0008;
    /// This record has a next version in the version chain (an UPDATE produced
    /// a newer version of the same logical tuple).
    pub const HAS_NEXT_VERSION: u16 = 0x0010;
    /// This record is the result of an UPDATE (as opposed to a fresh INSERT).
    pub const IS_UPDATED: u16 = 0x0020;
}

/// Version header prepended to every MVCC-managed record on a slotted page.
///
/// The header is exactly **16 bytes** in `#[repr(C)]` layout.  `xmin` and
/// `xmax` are stored as `u32` (truncated from the 64-bit [`TxId`] used
/// by the in-memory transaction system — see module-level docs).
///
/// # Layout (16 bytes, little-endian on disk)
///
/// ```text
/// 0x00  xmin              u32   low 32 bits of the TxId that created this version
/// 0x04  xmax              u32   low 32 bits of the TxId that deleted this version (0 = live)
/// 0x08  cid               u16   command-id within the creating transaction
/// 0x0A  infomask          u16   cached commit/abort flags (see infomask module)
/// 0x0C  next_version_ptr  u32   page-local byte offset to next version (0 = none)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct TupleHeader {
    /// Low 32 bits of the transaction ID that created (inserted) this version.
    pub xmin: u32,
    /// Low 32 bits of the transaction ID that deleted this version.
    /// `TUPLE_TXID_INVALID` (0) means the version is still live.
    pub xmax: u32,
    /// Command identifier within the creating transaction.  Allows a single
    /// transaction to see its own earlier commands without seeing its later
    /// ones — essential for savepoint-level visibility.
    pub cid: u16,
    /// Cached transaction status flags.  See [`infomask`].
    pub infomask: u16,
    /// Page-local byte offset (from start of page data area) to the next
    /// version of this tuple, or `0` if this is the latest version.
    pub next_version_ptr: u32,
}

impl TupleHeader {
    /// Size of the header in bytes.  Must remain exactly 16.
    pub const SIZE: usize = size_of::<TupleHeader>();

    /// Create a header for a freshly inserted tuple.
    ///
    /// * `xmin` — the transaction that performed the INSERT (truncated to u32).
    /// * `cid`  — the command ID within that transaction.
    pub fn new_insert(xmin: TxId, cid: u16) -> Self {
        Self {
            xmin: xmin as u32,
            xmax: TUPLE_TXID_INVALID,
            cid,
            infomask: 0,
            next_version_ptr: 0,
        }
    }

    /// Returns `true` if this version has not been logically deleted
    /// (i.e., `xmax` is the invalid sentinel).
    pub fn is_live(&self) -> bool {
        self.xmax == TUPLE_TXID_INVALID
    }

    /// Returns `true` if a deleter has been recorded (regardless of whether
    /// that deleter has committed or aborted).
    pub fn is_deleted(&self) -> bool {
        self.xmax != TUPLE_TXID_INVALID
    }

    /// Record that transaction `xmax` has deleted this version.
    ///
    /// The TxId is truncated to 32 bits for on-disk storage.
    ///
    /// This does **not** set the `XMAX_COMMITTED` flag — that is set later
    /// when the deleting transaction commits and the visibility evaluator
    /// caches the result.
    pub fn mark_deleted(&mut self, xmax: TxId) {
        debug_assert_ne!(xmax, TX_ID_INVALID, "cannot delete with an invalid TxId");
        self.xmax = xmax as u32;
    }

    /// Retrieve the creating TxId as a full 64-bit value.
    ///
    /// The epoch (high 32 bits) is not stored in the tuple header;
    /// callers that need epoch-correct comparisons must consult the
    /// global transaction state.
    pub fn xmin_txid(&self) -> TxId {
        self.xmin as TxId
    }

    /// Retrieve the deleting TxId as a full 64-bit value (0 = live).
    pub fn xmax_txid(&self) -> TxId {
        self.xmax as TxId
    }

    /// Set or clear an infomask flag.
    pub fn set_flag(&mut self, flag: u16) {
        self.infomask |= flag;
    }

    /// Clear an infomask flag.
    pub fn clear_flag(&mut self, flag: u16) {
        self.infomask &= !flag;
    }

    /// Test whether a flag is set.
    pub fn has_flag(&self, flag: u16) -> bool {
        self.infomask & flag != 0
    }

    /// Encode the header into `out`.
    ///
    /// # Panics
    ///
    /// Panics if `out.len() != TupleHeader::SIZE`.
    pub fn encode(&self, out: &mut [u8]) {
        assert_eq!(
            out.len(),
            Self::SIZE,
            "output buffer must be exactly TupleHeader::SIZE bytes"
        );
        // SAFETY: `TupleHeader` is `#[repr(C)]` with no implicit padding
        // (u32 + u32 + u16 + u16 + u32 = 16 bytes, all fields naturally
        // aligned within the struct).  `out` is exactly `SIZE` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const Self as *const u8,
                out.as_mut_ptr(),
                Self::SIZE,
            );
        }
    }

    /// Decode a header from `bytes`.
    ///
    /// Returns `None` if `bytes.len() < TupleHeader::SIZE`.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        // SAFETY: `bytes` is at least `SIZE` bytes long.  We use
        // `read_unaligned` because the record may not sit at a naturally
        // aligned address within the page buffer.
        Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const TupleHeader) })
    }
}

#[cfg(test)]
mod tests {
    use super::{infomask::*, *};

    #[test]
    fn header_size_is_16() {
        assert_eq!(TupleHeader::SIZE, 16);
        assert_eq!(size_of::<TupleHeader>(), 16);
    }

    #[test]
    fn new_insert_is_live() {
        let h = TupleHeader::new_insert(42, 0);
        assert!(h.is_live());
        assert!(!h.is_deleted());
        assert_eq!(h.xmin, 42u32);
        assert_eq!(h.xmax, TUPLE_TXID_INVALID);
        assert_eq!(h.infomask, 0);
        assert_eq!(h.next_version_ptr, 0);
    }

    #[test]
    fn mark_deleted_transitions_live_to_deleted() {
        let mut h = TupleHeader::new_insert(1, 0);
        assert!(h.is_live());
        h.mark_deleted(2);
        assert!(h.is_deleted());
        assert!(!h.is_live());
        assert_eq!(h.xmax, 2u32);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let mut h = TupleHeader::new_insert(100, 3);
        h.mark_deleted(200);
        h.set_flag(XMIN_COMMITTED | XMAX_COMMITTED);
        h.next_version_ptr = 512;

        let mut buf = [0u8; TupleHeader::SIZE];
        h.encode(&mut buf);
        let decoded = TupleHeader::decode(&buf).unwrap();

        assert_eq!(decoded.xmin, 100u32);
        assert_eq!(decoded.xmax, 200u32);
        assert_eq!(decoded.cid, 3);
        assert_eq!(decoded.infomask, XMIN_COMMITTED | XMAX_COMMITTED);
        assert_eq!(decoded.next_version_ptr, 512);
    }

    #[test]
    fn decode_returns_none_for_short_slice() {
        assert!(TupleHeader::decode(&[0u8; 15]).is_none());
    }

    #[test]
    fn decode_succeeds_for_larger_slice() {
        // decode only reads the first SIZE bytes; extra bytes are fine.
        let buf = [0u8; 32];
        assert!(TupleHeader::decode(&buf).is_some());
    }

    #[test]
    fn infomask_flag_combinations() {
        let mut h = TupleHeader::new_insert(1, 0);
        assert!(!h.has_flag(XMIN_COMMITTED));
        assert!(!h.has_flag(XMIN_ABORTED));

        h.set_flag(XMIN_COMMITTED);
        assert!(h.has_flag(XMIN_COMMITTED));
        assert!(!h.has_flag(XMIN_ABORTED));

        h.set_flag(XMAX_COMMITTED | HAS_NEXT_VERSION);
        assert!(h.has_flag(XMAX_COMMITTED));
        assert!(h.has_flag(HAS_NEXT_VERSION));
        assert!(!h.has_flag(IS_UPDATED));

        h.clear_flag(XMAX_COMMITTED);
        assert!(!h.has_flag(XMAX_COMMITTED));
        // Other flags must be undisturbed.
        assert!(h.has_flag(XMIN_COMMITTED));
        assert!(h.has_flag(HAS_NEXT_VERSION));
    }

    #[test]
    fn infomask_is_updated_flag() {
        let mut h = TupleHeader::new_insert(5, 0);
        h.set_flag(IS_UPDATED);
        assert!(h.has_flag(IS_UPDATED));
        h.clear_flag(IS_UPDATED);
        assert!(!h.has_flag(IS_UPDATED));
    }

    #[test]
    fn encode_panics_on_wrong_size() {
        let h = TupleHeader::new_insert(1, 0);
        let result = std::panic::catch_unwind(|| {
            let mut buf = [0u8; 8]; // wrong size
            h.encode(&mut buf);
        });
        assert!(result.is_err());
    }

    #[test]
    fn encode_decode_all_infomask_combinations() {
        // Exercise every defined flag to ensure no field overlaps.
        for flags in [
            XMIN_COMMITTED,
            XMIN_ABORTED,
            XMAX_COMMITTED,
            XMAX_ABORTED,
            HAS_NEXT_VERSION,
            IS_UPDATED,
            XMIN_COMMITTED | XMAX_COMMITTED,
            XMIN_ABORTED | XMAX_ABORTED,
        ] {
            let mut h = TupleHeader::new_insert(1, 0);
            h.infomask = flags;
            let mut buf = [0u8; TupleHeader::SIZE];
            h.encode(&mut buf);
            let d = TupleHeader::decode(&buf).unwrap();
            assert_eq!(d.infomask, flags);
        }
    }
}
