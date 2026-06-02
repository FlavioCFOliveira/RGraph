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
}
