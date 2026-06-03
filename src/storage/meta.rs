use crate::io::AlignedBuffer;
use crate::storage::page::{PageId, PAGE_SIZE};
use std::mem::size_of;

/// Current on-disk format version.
pub const FORMAT_VERSION: u32 = 1;

/// Magic signature for the database superblock.
pub const META_MAGIC: u64 = 0x5247_5241_5048_0001; // "RGRAPH\0\x01"

/// Fixed-size superblock stored at the start of the database file.
///
/// Two copies are kept (primary at offset 0, mirror at offset `PAGE_SIZE`)
/// so that a torn write of one copy can be recovered from the other.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Superblock {
    /// Magic signature (`META_MAGIC`).
    pub magic: u64,
    /// Format version.
    pub version: u32,
    /// Size of each page in bytes (must be `PAGE_SIZE`).
    pub page_size: u32,
    /// Total number of pages ever allocated.
    pub total_page_count: u64,
    /// Number of pages currently free.
    pub free_page_count: u64,
    /// Next page id to hand out if the free list is empty.
    pub next_free_page_id: PageId,
    /// LSN of the most recently written WAL record.
    pub current_wal_lsn: u64,
    /// LSN of the last completed checkpoint.
    pub last_checkpoint_lsn: u64,
    /// File offset (in bytes) of the dirty-page table from the last
    /// checkpoint.  Zero if no checkpoint has been taken.
    pub checkpoint_dirty_page_table_offset: u64,
    /// Generation counter used to decide which copy (primary or mirror)
    /// is newer during recovery.
    pub generation: u64,
    /// CRC32C of everything above, computed with `checksum` = 0.
    pub checksum: u32,
    /// Reserved padding to keep the struct at 128 bytes.
    pub _reserved: [u8; 52],
}

impl Superblock {
    pub fn new(page_size: u32) -> Self {
        Self {
            magic: META_MAGIC,
            version: FORMAT_VERSION,
            page_size,
            total_page_count: 0,
            free_page_count: 0,
            next_free_page_id: 1, // page 0 is the superblock
            current_wal_lsn: 0,
            last_checkpoint_lsn: 0,
            checkpoint_dirty_page_table_offset: 0,
            generation: 1,
            checksum: 0,
            _reserved: [0; 52],
        }
    }

    pub fn compute_checksum(&self) -> u32 {
        let mut copy = *self;
        copy.checksum = 0;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &copy as *const _ as *const u8,
                size_of::<Superblock>(),
            )
        };
        crc32c::crc32c(bytes)
    }

    pub fn update_checksum(&mut self) {
        self.checksum = self.compute_checksum();
    }

    pub fn verify_checksum(&self) -> bool {
        self.checksum == self.compute_checksum()
    }

    pub fn is_valid(&self) -> bool {
        self.magic == META_MAGIC
            && self.version == FORMAT_VERSION
            && self.page_size == PAGE_SIZE as u32
            && self.verify_checksum()
    }
}

/// Write a superblock into a page-aligned buffer suitable for I/O.
pub fn encode_superblock(sb: &Superblock) -> AlignedBuffer {
    let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
    let bytes = unsafe {
        std::slice::from_raw_parts(
            sb as *const _ as *const u8,
            size_of::<Superblock>(),
        )
    };
    buf[..bytes.len()].copy_from_slice(bytes);
    buf
}

/// Decode a superblock from a raw page buffer.
pub fn decode_superblock(buf: &[u8]) -> Option<Superblock> {
    if buf.len() < size_of::<Superblock>() {
        return None;
    }
    let sb = unsafe {
        std::ptr::read_unaligned(buf.as_ptr() as *const Superblock)
    };
    if sb.is_valid() {
        Some(sb)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superblock_size_is_128() {
        assert_eq!(size_of::<Superblock>(), 128);
    }

    #[test]
    fn roundtrip() {
        let mut sb = Superblock::new(PAGE_SIZE as u32);
        sb.total_page_count = 42;
        sb.update_checksum();
        let buf = encode_superblock(&sb);
        let decoded = decode_superblock(&buf).unwrap();
        assert_eq!(decoded.magic, META_MAGIC);
        assert_eq!(decoded.total_page_count, 42);
    }

    #[test]
    fn corruption_detected() {
        let mut sb = Superblock::new(PAGE_SIZE as u32);
        sb.update_checksum();
        let mut buf = encode_superblock(&sb);
        buf[20] ^= 0xFF;
        assert!(decode_superblock(&buf).is_none());
    }
}
