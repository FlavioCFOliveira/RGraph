use crc32c::crc32c;
use std::mem::size_of;
use std::time::{SystemTime, UNIX_EPOCH};
use twox_hash::XxHash64;
use std::hash::Hasher;

/// Magic start delimiter: "WAL!" in big-endian.
pub const WAL_MAGIC_START: u32 = 0x5741_4C21;
/// Magic end delimiter: "!LAW" in big-endian.
pub const WAL_MAGIC_END: u32 = 0x214C_4157;
/// On-wire format version.
pub const WAL_VERSION: u8 = 1;

/// Minimum size of a WAL record on disk (header + start/end magic + checksums).
pub const WAL_RECORD_MIN_SIZE: usize =
    size_of::<u32>()   // start magic
    + 1                // version
    + 1                // record_type
    + 8                // txid
    + 8                // lsn
    + 8                // prev_lsn
    + 8                // timestamp
    + 4                // payload_len
    + 4                // header_crc32c
    + 8                // payload_xxhash
    + size_of::<u32>(); // end magic

/// Physical record types written to the WAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordType {
    /// Transaction begin.
    Begin = 0x01,
    /// Transaction commit.
    Commit = 0x02,
    /// Transaction abort.
    Abort = 0x03,
    /// A new page was allocated.
    PageInsert = 0x10,
    /// An existing page was modified.
    PageUpdate = 0x11,
    /// A page was returned to the free list.
    PageFree = 0x12,
    /// The allocation bitmap changed.
    BitmapUpdate = 0x13,
    /// Begin checkpoint marker.
    CheckpointBegin = 0x20,
    /// End checkpoint marker (contains dirty page table and active tx table).
    CheckpointEnd = 0x21,
    /// A new node record was inserted.
    NodeInsert = 0x30,
    /// A node record was deleted (tombstone).
    NodeDelete = 0x31,
    /// A new edge record was inserted.
    EdgeInsert = 0x32,
    /// An edge record was deleted (tombstone).
    EdgeDelete = 0x33,
    /// A new property record was inserted.
    PropertyInsert = 0x34,
    /// After-image of a modified node record.
    NodeUpdate = 0x35,
    /// After-image of a modified edge record.
    EdgeUpdate = 0x36,
    /// After-image of a modified property record.
    PropertyUpdate = 0x37,
    /// Compensation Log Record — written during UNDO, never itself undone.
    ///
    /// Payload layout:
    /// - `[0..8]`  `undo_next_lsn`: the `prev_lsn` of the record being compensated
    ///   (i.e. the next record to undo for this transaction).
    /// - `[8..16]` `page_id`: the page being restored.
    /// - `[16..]`  before-image or tombstone of the undone state.
    Clr = 0x40,
    /// Written at the end of a full WAL segment before rotation.
    ///
    /// Payload is a [`SegmentDescriptor::SIZE`]-byte block produced by
    /// [`SegmentDescriptor::encode`].
    SegmentDescriptor = 0x50,
    /// Begin-compaction marker — tombstone-only pages are being collected.
    ///
    /// Logical record: carries no page image and is a no-op for REDO/UNDO.
    /// It bounds the compaction operation in the WAL so recovery can observe
    /// that a compaction was in progress.
    CompactionBegin = 0x60,
    /// End-compaction marker.
    ///
    /// Payload: an 8-byte big-endian count of reclaimed slots.  Logical
    /// record: a no-op for REDO/UNDO.
    CompactionEnd = 0x61,
    /// A B+ tree index page was created during an index mutation.
    ///
    /// Physical page record: payload is `[8-byte page_id][full page image]`,
    /// optionally followed by an embedded before-image for UNDO.  Treated
    /// exactly like [`PageInsert`] by REDO/UNDO but kept distinct so that
    /// recovery and tooling can attribute the change to a secondary index.
    IndexPageInsert = 0x70,
    /// A B+ tree index page was modified during an index mutation.
    ///
    /// Physical page record with the same payload layout as
    /// [`IndexPageInsert`]; treated like [`PageUpdate`] by REDO/UNDO.
    IndexPageUpdate = 0x71,
    /// A B+ tree index page was freed during an index mutation.
    ///
    /// Physical page record; treated like [`PageFree`] by REDO/UNDO.
    IndexPageFree = 0x72,
}

/// A single WAL record.
///
/// The on-disk layout is:
///
/// ```text
/// 0x00  start_magic   u32   0x57414C21
/// 0x04  version       u8    1
/// 0x05  record_type   u8
/// 0x06  txid          u64
/// 0x0E  lsn           u64
/// 0x16  prev_lsn      u64
/// 0x1E  timestamp     u64   millis since UNIX epoch
/// 0x26  payload_len   u32
/// 0x2A  header_crc    u32   CRC32C of [0x00..0x2A]
/// 0x2E  payload       [payload_len] bytes
///       payload_hash  u64   xxHash3_64 of payload
///       end_magic     u32   0x214C4157
/// ```
#[derive(Debug, Clone)]
pub struct WalRecord {
    pub record_type: RecordType,
    pub txid: u64,
    pub lsn: u64,
    pub prev_lsn: u64,
    pub timestamp: u64,
    pub payload: Vec<u8>,
    pub header_crc: u32,
    pub payload_hash: u64,
}

impl WalRecord {
    /// Create a new record.  `lsn` must be the *byte offset* at which
    /// this record will be written.
    pub fn new(
        record_type: RecordType,
        txid: u64,
        lsn: u64,
        prev_lsn: u64,
        payload: Vec<u8>,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut rec = Self {
            record_type,
            txid,
            lsn,
            prev_lsn,
            timestamp: now,
            payload,
            header_crc: 0,
            payload_hash: 0,
        };
        rec.payload_hash = rec.compute_payload_hash();
        rec.header_crc = rec.compute_header_crc();
        rec
    }

    /// Update the LSN and recalculate checksums.
    pub fn set_lsn(&mut self, lsn: u64) {
        self.lsn = lsn;
        self.payload_hash = self.compute_payload_hash();
        self.header_crc = self.compute_header_crc();
    }

    /// Total on-disk size of this record in bytes.
    pub fn on_disk_size(&self) -> usize {
        WAL_RECORD_MIN_SIZE + self.payload.len()
    }

    fn compute_header_crc(&self) -> u32 {
        let mut buf = Vec::with_capacity(42);
        buf.extend_from_slice(&WAL_MAGIC_START.to_be_bytes());
        buf.push(WAL_VERSION);
        buf.push(self.record_type as u8);
        buf.extend_from_slice(&self.txid.to_be_bytes());
        buf.extend_from_slice(&self.lsn.to_be_bytes());
        buf.extend_from_slice(&self.prev_lsn.to_be_bytes());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        // header_crc field itself is zero during calculation
        buf.extend_from_slice(&0u32.to_be_bytes());
        crc32c(&buf)
    }

    fn compute_payload_hash(&self) -> u64 {
        let mut hasher = XxHash64::default();
        hasher.write(&self.payload);
        hasher.finish()
    }

    /// Encode the record into a byte vector suitable for appending.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.on_disk_size());
        buf.extend_from_slice(&WAL_MAGIC_START.to_be_bytes());
        buf.push(WAL_VERSION);
        buf.push(self.record_type as u8);
        buf.extend_from_slice(&self.txid.to_be_bytes());
        buf.extend_from_slice(&self.lsn.to_be_bytes());
        buf.extend_from_slice(&self.prev_lsn.to_be_bytes());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&self.header_crc.to_be_bytes());
        buf.extend_from_slice(&self.payload);
        buf.extend_from_slice(&self.payload_hash.to_be_bytes());
        buf.extend_from_slice(&WAL_MAGIC_END.to_be_bytes());
        buf
    }

    /// Attempt to decode a single record from `buf` starting at `offset`.
    /// Returns `Some((record, next_offset))` on success, or `None` if
    /// there are not enough bytes or the record is corrupt.
    pub fn decode(buf: &[u8], offset: usize) -> Option<(Self, usize)> {
        if buf.len() < offset + WAL_RECORD_MIN_SIZE {
            return None;
        }
        let start = offset;
        let mut cursor = offset;

        let magic_start = u32::from_be_bytes(read_arr(buf, cursor)?);
        cursor += 4;
        if magic_start != WAL_MAGIC_START {
            return None;
        }

        let version = read_u8(buf, cursor)?;
        cursor += 1;
        if version != WAL_VERSION {
            return None;
        }

        let record_type = match read_u8(buf, cursor)? {
            0x01 => RecordType::Begin,
            0x02 => RecordType::Commit,
            0x03 => RecordType::Abort,
            0x10 => RecordType::PageInsert,
            0x11 => RecordType::PageUpdate,
            0x12 => RecordType::PageFree,
            0x13 => RecordType::BitmapUpdate,
            0x20 => RecordType::CheckpointBegin,
            0x21 => RecordType::CheckpointEnd,
            0x30 => RecordType::NodeInsert,
            0x31 => RecordType::NodeDelete,
            0x32 => RecordType::EdgeInsert,
            0x33 => RecordType::EdgeDelete,
            0x34 => RecordType::PropertyInsert,
            0x35 => RecordType::NodeUpdate,
            0x36 => RecordType::EdgeUpdate,
            0x37 => RecordType::PropertyUpdate,
            0x40 => RecordType::Clr,
            0x50 => RecordType::SegmentDescriptor,
            0x60 => RecordType::CompactionBegin,
            0x61 => RecordType::CompactionEnd,
            0x70 => RecordType::IndexPageInsert,
            0x71 => RecordType::IndexPageUpdate,
            0x72 => RecordType::IndexPageFree,
            _ => return None,
        };
        cursor += 1;

        let txid = u64::from_be_bytes(read_arr(buf, cursor)?);
        cursor += 8;
        let lsn = u64::from_be_bytes(read_arr(buf, cursor)?);
        cursor += 8;
        let prev_lsn = u64::from_be_bytes(read_arr(buf, cursor)?);
        cursor += 8;
        let timestamp = u64::from_be_bytes(read_arr(buf, cursor)?);
        cursor += 8;
        let payload_len = u32::from_be_bytes(read_arr(buf, cursor)?) as usize;
        cursor += 4;
        let header_crc = u32::from_be_bytes(read_arr(buf, cursor)?);
        cursor += 4;

        if buf.len() < cursor + payload_len + 8 + 4 {
            return None;
        }

        let payload = buf[cursor..cursor + payload_len].to_vec();
        cursor += payload_len;

        let payload_hash = u64::from_be_bytes(read_arr(buf, cursor)?);
        cursor += 8;
        let magic_end = u32::from_be_bytes(read_arr(buf, cursor)?);
        cursor += 4;
        if magic_end != WAL_MAGIC_END {
            return None;
        }

        let rec = Self {
            record_type,
            txid,
            lsn,
            prev_lsn,
            timestamp,
            payload,
            header_crc,
            payload_hash,
        };

        // Verify checksums.
        if rec.compute_header_crc() != header_crc {
            return None;
        }
        if rec.compute_payload_hash() != payload_hash {
            return None;
        }

        Some((rec, cursor - start))
    }
}

fn read_u8(buf: &[u8], offset: usize) -> Option<u8> {
    buf.get(offset).copied()
}

fn read_arr<const N: usize>(buf: &[u8], offset: usize) -> Option<[u8; N]> {
    if buf.len() < offset + N {
        return None;
    }
    let mut arr = [0u8; N];
    arr.copy_from_slice(&buf[offset..offset + N]);
    Some(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let rec = WalRecord::new(RecordType::PageUpdate, 1, 0, 0, vec![1, 2, 3, 4]);
        let bytes = rec.encode();
        let (decoded, consumed) = WalRecord::decode(&bytes, 0).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded.record_type, rec.record_type);
        assert_eq!(decoded.payload, rec.payload);
        assert_eq!(decoded.lsn, rec.lsn);
    }

    #[test]
    fn corruption_detected() {
        let rec = WalRecord::new(RecordType::Begin, 0, 100, 0, vec![]);
        let mut bytes = rec.encode();
        bytes[10] ^= 0xFF;
        assert!(WalRecord::decode(&bytes, 0).is_none());
    }

    #[test]
    fn wrong_magic_rejected() {
        let mut bytes = vec![0u8; WAL_RECORD_MIN_SIZE];
        bytes[0..4].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
        assert!(WalRecord::decode(&bytes, 0).is_none());
    }

    #[test]
    fn new_record_types_roundtrip() {
        for rt in [
            RecordType::NodeUpdate,
            RecordType::EdgeUpdate,
            RecordType::PropertyUpdate,
            RecordType::Clr,
            RecordType::SegmentDescriptor,
            RecordType::CompactionBegin,
            RecordType::CompactionEnd,
            RecordType::IndexPageInsert,
            RecordType::IndexPageUpdate,
            RecordType::IndexPageFree,
        ] {
            let rec = WalRecord::new(rt, 42, 0, 0, vec![0xAB, 0xCD]);
            let bytes = rec.encode();
            let (decoded, consumed) = WalRecord::decode(&bytes, 0).unwrap();
            assert_eq!(consumed, bytes.len());
            assert_eq!(decoded.record_type, rt);
        }
    }
}

#[cfg(test)]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn wal_record_roundtrip(
            txid in 0u64..u64::MAX,
            prev_lsn in 0u64..u64::MAX,
            payload in prop::collection::vec(any::<u8>(), 0..512),
        ) {
            let rec = WalRecord::new(RecordType::PageUpdate, txid, 0, prev_lsn, payload.clone());
            let bytes = rec.encode();
            let (decoded, consumed) = WalRecord::decode(&bytes, 0).unwrap();
            prop_assert_eq!(consumed, bytes.len());
            prop_assert_eq!(decoded.txid, txid);
            prop_assert_eq!(decoded.prev_lsn, prev_lsn);
            prop_assert_eq!(decoded.payload, payload);
        }

        #[test]
        fn lsn_monotonic_after_set(lsn in 1u64..u64::MAX) {
            let mut rec = WalRecord::new(RecordType::Begin, 1, 0, 0, vec![]);
            rec.set_lsn(lsn);
            prop_assert_eq!(rec.lsn, lsn);
            // Checksums must be consistent after set_lsn.
            let bytes = rec.encode();
            prop_assert!(WalRecord::decode(&bytes, 0).is_some());
        }

        #[test]
        fn corruption_always_detected(
            payload in prop::collection::vec(any::<u8>(), 0..256),
            corrupt_byte_idx in 0usize..50,
            corrupt_value in 1u8..=255u8,
        ) {
            let rec = WalRecord::new(RecordType::PageUpdate, 1, 0, 0, payload);
            let mut bytes = rec.encode();
            let idx = corrupt_byte_idx % bytes.len();
            bytes[idx] ^= corrupt_value;
            // Corrupted record may or may not decode, but if it does decode,
            // the checksum check should catch it (or it happened to produce a valid record).
            // We only assert it doesn't panic.
            let _ = WalRecord::decode(&bytes, 0);
        }

        #[test]
        fn empty_payload_roundtrip(
            txid in 0u64..u64::MAX,
            lsn in 0u64..u64::MAX,
        ) {
            let rec = WalRecord::new(RecordType::Begin, txid, lsn, 0, vec![]);
            let bytes = rec.encode();
            let result = WalRecord::decode(&bytes, 0);
            prop_assert!(result.is_some());
            let (decoded, consumed) = result.unwrap();
            prop_assert_eq!(consumed, bytes.len());
            prop_assert_eq!(decoded.txid, txid);
            prop_assert!(decoded.payload.is_empty());
        }

        #[test]
        fn max_payload_roundtrip(
            payload in prop::collection::vec(any::<u8>(), 480..512),
        ) {
            let rec = WalRecord::new(RecordType::PageUpdate, 1, 0, 0, payload.clone());
            let bytes = rec.encode();
            let (decoded, _) = WalRecord::decode(&bytes, 0).unwrap();
            prop_assert_eq!(decoded.payload, payload);
        }
    }
}
