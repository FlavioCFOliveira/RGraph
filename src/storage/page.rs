use crate::io::AlignedBuffer;
use std::mem::size_of;

/// Size of a database page in bytes.
pub const PAGE_SIZE: usize = 8192;

/// Maximum fraction of a page a single record may occupy before it is
/// moved to an overflow chain.
pub const MAX_INLINE_RECORD_RATIO: f64 = 0.25;

/// Maximum inline record size in bytes.
pub const MAX_INLINE_RECORD_LEN: usize = (PAGE_SIZE as f64 * MAX_INLINE_RECORD_RATIO) as usize;

/// Magic value identifying a valid page header.
pub const PAGE_MAGIC: u32 = 0x5247_5047; // "RGPG"

/// A typed page identifier.
pub type PageId = u64;

/// Discriminant for the kind of data stored in a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    /// Database superblock / meta page.
    Meta = 0x01,
    /// Allocation bitmap page.
    Bitmap = 0x02,
    /// B+ tree node (interior).
    BTreeInterior = 0x10,
    /// B+ tree leaf.
    BTreeLeaf = 0x11,
    /// Raw slotted data page (graph records).
    SlottedData = 0x20,
    /// Overflow continuation page.
    Overflow = 0x30,
}

/// Fixed-size header at the start of every 8 KiB page.
///
/// Total header size is **64 bytes**, leaving **8128 bytes** for records
/// and the slot directory.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct PageHeader {
    /// Logical sequence number of the most recent WAL record that
    /// modified this page.  Used for idempotent redo.
    pub page_lsn: u64,
    /// Monotonically increasing page identifier.
    pub page_id: PageId,
    /// For overflow pages: the next page in the chain, or `0`.
    pub overflow_page_id: PageId,
    /// Magic signature (`PAGE_MAGIC`).
    pub magic: u32,
    /// CRC32C of the entire page (header + payload), computed with this
    /// field set to zero.
    pub checksum: u32,
    /// Byte offset (from start of page data area) to the first free byte.
    pub free_space_offset: u16,
    /// Number of slots currently in the slot directory.
    pub slot_count: u16,
    /// Bit flags (e.g. `PAGE_FLAG_FULL`).
    pub flags: u16,
    /// Reserved.
    pub _pad2: u16,
    /// Format version of this page.
    pub version: u8,
    /// Page type discriminant.
    pub page_type: u8,
    /// Reserved / padding.
    pub _pad1: u16,
    /// Reserved tail of header to keep size at 64 bytes.
    pub _reserved: [u8; 20],
}

/// Bit flag: page is considered full (no further insertions attempted).
pub const PAGE_FLAG_FULL: u16 = 0x0001;

impl PageHeader {
    /// Create a new header for the given page id and type.
    pub fn new(page_id: PageId, page_type: PageType) -> Self {
        Self {
            page_lsn: 0,
            page_id,
            overflow_page_id: 0,
            magic: PAGE_MAGIC,
            checksum: 0,
            free_space_offset: 0,
            slot_count: 0,
            flags: 0,
            _pad2: 0,
            version: 1,
            page_type: page_type as u8,
            _pad1: 0,
            _reserved: [0; 20],
        }
    }

    /// Number of bytes available in the data area.
    pub fn free_space(&self) -> usize {
        // data area = PAGE_SIZE - header - slot directory
        let header_size = size_of::<PageHeader>();
        let slot_dir_size = self.slot_count as usize * size_of::<Slot>();
        let used = header_size + self.free_space_offset as usize + slot_dir_size;
        PAGE_SIZE.saturating_sub(used)
    }
}

/// A single entry in the backward-growing slot directory.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Slot {
    /// Byte offset of the record within the page data area.
    pub offset: u16,
    /// Length of the record in bytes.
    pub length: u16,
}

impl Slot {
    /// Sentinel value indicating a deleted slot.
    pub const DELETED: Slot = Slot {
        offset: u16::MAX,
        length: 0,
    };

    /// Is this slot deleted?
    pub fn is_deleted(&self) -> bool {
        self.offset == u16::MAX
    }
}

/// In-memory view of a slotted page backed by an [`AlignedBuffer`].
#[derive(Debug, Clone)]
pub struct SlottedPage {
    pub buf: AlignedBuffer,
}

impl SlottedPage {
    /// Number of header bytes.
    pub const HEADER_SIZE: usize = size_of::<PageHeader>();
    /// Start of the slot directory relative to page start.
    pub const SLOT_DIR_START: usize = PAGE_SIZE - size_of::<Slot>() * (u16::MAX as usize);

    /// Wrap an existing buffer.
    pub fn new(buf: AlignedBuffer) -> Self {
        assert_eq!(buf.len(), PAGE_SIZE, "slotted page must be PAGE_SIZE");
        Self { buf }
    }

    /// Initialise a blank page with the given id and type.
    pub fn init(page_id: PageId, page_type: PageType) -> Self {
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        let header = PageHeader::new(page_id, page_type);
        // SAFETY: `buf` is at least `HEADER_SIZE` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                &header as *const _ as *const u8,
                buf.as_mut_ptr(),
                Self::HEADER_SIZE,
            );
        }
        Self::new(buf)
    }

    /// Mutable reference to the header.
    pub fn header(&self) -> &PageHeader {
        // SAFETY: `buf` is aligned and large enough.
        unsafe { &*(self.buf.as_ptr() as *const PageHeader) }
    }

    /// Mutable reference to the header.
    pub fn header_mut(&mut self) -> &mut PageHeader {
        // SAFETY: `buf` is aligned and large enough and we own it exclusively.
        unsafe { &mut *(self.buf.as_mut_ptr() as *mut PageHeader) }
    }

    /// Compute the CRC32C of the entire page (header with checksum=0 + payload).
    pub fn compute_checksum(&self) -> u32 {
        let mut copy = *self.header();
        copy.checksum = 0;
        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                &copy as *const _ as *const u8,
                size_of::<PageHeader>(),
            )
        };
        let data = &self.buf[Self::HEADER_SIZE..];
        let mut buf = Vec::with_capacity(header_bytes.len() + data.len());
        buf.extend_from_slice(header_bytes);
        buf.extend_from_slice(data);
        crc32c::crc32c(&buf)
    }

    /// Verify the stored checksum.
    pub fn verify_checksum(&self) -> bool {
        self.header().checksum == self.compute_checksum()
    }

    /// Recalculate and store the checksum.
    pub fn update_checksum(&mut self) {
        let cksum = self.compute_checksum();
        self.header_mut().checksum = cksum;
    }

    /// Slice of the current slot directory (tail of the page).
    ///
    /// Slot 0 is the *last* slot in the buffer (closest to PAGE_SIZE),
    /// slot 1 is the one before it, etc.  This mirrors the traditional
    /// backward-growing slot directory.
    fn slot_dir(&self) -> &[Slot] {
        let count = self.header().slot_count as usize;
        if count == 0 {
            return &[];
        }
        let start = PAGE_SIZE - count * size_of::<Slot>();
        let bytes = &self.buf[start..PAGE_SIZE];
        // Slots are stored in reverse order in the buffer:
        // buf[PAGE_SIZE-4..] = slot 0, buf[PAGE_SIZE-8..PAGE_SIZE-4] = slot 1, ...
        unsafe {
            std::slice::from_raw_parts(bytes.as_ptr() as *const Slot, count)
        }
    }

    /// Access a slot by index.
    ///
    /// Slot 0 is stored closest to the end of the page (highest address).
    pub fn slot(&self, idx: u16) -> Option<&Slot> {
        let count = self.header().slot_count as usize;
        let rev = count.checked_sub((idx as usize) + 1)?;
        self.slot_dir().get(rev)
    }

    /// Mutable access to a slot by index.
    pub fn slot_mut(&mut self, idx: u16) -> Option<&mut Slot> {
        let count = self.header().slot_count as usize;
        if (idx as usize) >= count {
            return None;
        }
        let start = PAGE_SIZE - count * size_of::<Slot>();
        let bytes = &mut self.buf[start..PAGE_SIZE];
        let slots = unsafe {
            std::slice::from_raw_parts_mut(bytes.as_mut_ptr() as *mut Slot, count)
        };
        let rev = count - (idx as usize) - 1;
        slots.get_mut(rev)
    }

    /// Insert a record and return its slot index, or `None` if it does not fit.
    pub fn insert(&mut self, record: &[u8]) -> Option<u16> {
        if record.len() > MAX_INLINE_RECORD_LEN {
            return None; // must go to overflow
        }
        let header = self.header();
        let slot_count = header.slot_count as usize;
        let free_offset = header.free_space_offset as usize;
        let slot_dir_size = slot_count * size_of::<Slot>();
        let needed = record.len() + size_of::<Slot>();
        let used = Self::HEADER_SIZE + free_offset + slot_dir_size;
        let available = PAGE_SIZE.saturating_sub(used);

        if needed > available {
            // Try compacting deleted slots first.
            self.compact();
            let header = self.header();
            let slot_count = header.slot_count as usize;
            let free_offset = header.free_space_offset as usize;
            let slot_dir_size = slot_count * size_of::<Slot>();
            let used = Self::HEADER_SIZE + free_offset + slot_dir_size;
            let available = PAGE_SIZE.saturating_sub(used);
            if needed > available {
                return None;
            }
        }

        let mut best_idx: Option<u16> = None;
        let mut best_len = usize::MAX;

        // best-fit among deleted slots
        for i in 0..slot_count {
            if let Some(slot) = self.slot(i as u16)
                && slot.is_deleted()
                && slot.length as usize >= record.len()
                && (slot.length as usize) < best_len
            {
                best_len = slot.length as usize;
                best_idx = Some(i as u16);
            }
        }

        if let Some(idx) = best_idx {
            // Reuse the directory entry: append data at current free offset
            // and update the slot to point there.
            let data_start = Self::HEADER_SIZE + free_offset;
            self.buf[data_start..data_start + record.len()].copy_from_slice(record);
            let slot = self.slot_mut(idx).unwrap();
            slot.offset = free_offset as u16;
            slot.length = record.len() as u16;
            let header_mut = self.header_mut();
            header_mut.free_space_offset = (free_offset + record.len()) as u16;
            let _ = header_mut;
            return Some(idx);
        }

        // Append at the end of the data area.
        let data_start = Self::HEADER_SIZE + free_offset;
        self.buf[data_start..data_start + record.len()].copy_from_slice(record);

        let header_mut = self.header_mut();
        header_mut.slot_count += 1;
        let new_slot_idx = header_mut.slot_count - 1;
        let new_offset = free_offset as u16;
        header_mut.free_space_offset = (free_offset + record.len()) as u16;
        let _ = header_mut;

        // Expand slot directory by one entry at the tail.
        let count = self.header().slot_count as usize;
        let start = PAGE_SIZE - count * size_of::<Slot>();
        let slot_bytes = &mut self.buf[start..start + size_of::<Slot>()];
        let slot = unsafe { &mut *(slot_bytes.as_mut_ptr() as *mut Slot) };
        slot.offset = new_offset;
        slot.length = record.len() as u16;

        Some(new_slot_idx)
    }

    /// Insert a record at a specific logical slot index, shifting existing
    /// slots to the right.  Returns `Some(idx)` on success or `None` if the
    /// record does not fit.
    pub fn insert_at(&mut self, idx: u16, record: &[u8]) -> Option<u16> {
        if record.len() > MAX_INLINE_RECORD_LEN {
            return None;
        }
        let header = self.header();
        let slot_count = header.slot_count as usize;
        let free_offset = header.free_space_offset as usize;
        let slot_dir_size = slot_count * size_of::<Slot>();
        let needed = record.len() + size_of::<Slot>();
        let used = Self::HEADER_SIZE + free_offset + slot_dir_size;
        let available = PAGE_SIZE.saturating_sub(used);

        let mut free_offset = free_offset;
        if needed > available {
            self.compact();
            let header = self.header();
            let slot_count = header.slot_count as usize;
            free_offset = header.free_space_offset as usize;
            let slot_dir_size = slot_count * size_of::<Slot>();
            let used = Self::HEADER_SIZE + free_offset + slot_dir_size;
            let available = PAGE_SIZE.saturating_sub(used);
            if needed > available {
                return None;
            }
        }

        // Write record at current free offset.
        let data_start = Self::HEADER_SIZE + free_offset;
        self.buf[data_start..data_start + record.len()].copy_from_slice(record);

        let header_mut = self.header_mut();
        header_mut.slot_count += 1;
        header_mut.free_space_offset = (free_offset + record.len()) as u16;
        let new_slot = Slot {
            offset: free_offset as u16,
            length: record.len() as u16,
        };

        // Collect existing slots in logical order (slot 0 .. slot N-1).
        let count = self.header().slot_count as usize;
        let mut slots: Vec<Slot> = Vec::with_capacity(count);
        for i in 0..(count - 1) {
            if let Some(s) = self.slot(i as u16) {
                slots.push(*s);
            }
        }

        // Insert new slot at the requested logical position.
        let pos = (idx as usize).min(slots.len());
        slots.insert(pos, new_slot);

        // Rewrite slot directory in reverse order (logical 0 at the tail).
        let start = PAGE_SIZE - count * size_of::<Slot>();
        for (i, slot) in slots.iter().enumerate() {
            let phys_pos = count - 1 - i;
            let offset = start + phys_pos * size_of::<Slot>();
            let bytes = unsafe {
                std::slice::from_raw_parts(slot as *const _ as *const u8, size_of::<Slot>())
            };
            self.buf[offset..offset + size_of::<Slot>()].copy_from_slice(bytes);
        }

        Some(pos as u16)
    }

    /// Delete the record at `idx`.  The slot is marked deleted but the
    /// data is not moved until compaction.  The original `length` is
    /// preserved so best-fit reuse can pick the smallest adequate slot.
    pub fn delete(&mut self, idx: u16) -> bool {
        if let Some(slot) = self.slot_mut(idx)
            && !slot.is_deleted()
        {
            slot.offset = u16::MAX;
            return true;
        }
        false
    }

    /// Read the record at `idx`.
    pub fn read(&self, idx: u16) -> Option<&[u8]> {
        let slot = self.slot(idx)?;
        if slot.is_deleted() {
            return None;
        }
        let start = Self::HEADER_SIZE + slot.offset as usize;
        let end = start + slot.length as usize;
        Some(&self.buf[start..end])
    }

    /// Compact the page: move all live records to the front and rebuild
    /// the slot directory without holes.
    pub fn compact(&mut self) {
        let old_count = self.header().slot_count;
        let mut new_offset = 0u16;
        let mut new_slots: Vec<Slot> = Vec::with_capacity(old_count as usize);

        // First pass: collect live slots without mutating buf.
        let mut moves: Vec<(usize, usize, usize)> = Vec::new();
        for i in 0..old_count {
            if let Some(slot) = self.slot(i)
                && !slot.is_deleted()
            {
                let old_start = Self::HEADER_SIZE + slot.offset as usize;
                let len = slot.length as usize;
                debug_assert!(
                    old_start + len <= PAGE_SIZE,
                    "corrupted slot {}: offset={}, length={}, page_id={}",
                    i,
                    slot.offset,
                    slot.length,
                    self.header().page_id
                );
                let new_start = Self::HEADER_SIZE + new_offset as usize;
                if old_start != new_start {
                    moves.push((old_start, new_start, len));
                }
                new_slots.push(Slot {
                    offset: new_offset,
                    length: slot.length,
                });
                new_offset += len as u16;
            }
        }

        // Second pass: mutate buf.
        for (old_start, new_start, len) in moves {
            self.buf.copy_within(old_start..old_start + len, new_start);
        }

        let header = self.header_mut();
        header.free_space_offset = new_offset;
        header.slot_count = new_slots.len() as u16;
        header.flags &= !PAGE_FLAG_FULL;
        let _ = header;

        // Write new slot directory at tail.
        let slot_dir_start = PAGE_SIZE - new_slots.len() * size_of::<Slot>();
        let slot_bytes = unsafe {
            std::slice::from_raw_parts(
                new_slots.as_ptr() as *const u8,
                new_slots.len() * size_of::<Slot>(),
            )
        };
        self.buf[slot_dir_start..slot_dir_start + slot_bytes.len()].copy_from_slice(slot_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_size_is_64() {
        assert_eq!(size_of::<PageHeader>(), 64);
    }

    #[test]
    fn init_and_verify_checksum() {
        let mut page = SlottedPage::init(42, PageType::SlottedData);
        page.update_checksum();
        assert!(page.verify_checksum());
    }

    #[test]
    fn insert_and_read_roundtrip() {
        let mut page = SlottedPage::init(1, PageType::SlottedData);
        let data = b"hello, world";
        let idx = page.insert(data).unwrap();
        assert_eq!(page.read(idx).unwrap(), data);
    }

    #[test]
    fn delete_makes_record_unreadable() {
        let mut page = SlottedPage::init(1, PageType::SlottedData);
        let idx = page.insert(b"bye").unwrap();
        assert!(page.delete(idx));
        assert!(page.read(idx).is_none());
    }

    #[test]
    fn best_fit_reuses_deleted_slot() {
        let mut page = SlottedPage::init(1, PageType::SlottedData);
        let idx = page.insert(b"12345").unwrap();
        page.delete(idx);
        let idx2 = page.insert(b"abc").unwrap();
        assert_eq!(idx, idx2); // should reuse slot 0
    }

    #[test]
    fn compact_reclaims_space() {
        let mut page = SlottedPage::init(1, PageType::SlottedData);
        page.insert(b"aaaaaaaaaa").unwrap();
        page.insert(b"bbbbbbbbbb").unwrap();
        page.delete(0);
        let before = page.header().free_space_offset;
        page.compact();
        let after = page.header().free_space_offset;
        assert!(after < before);
        // After compact the live record is now at slot 0.
        assert_eq!(page.read(0).unwrap(), b"bbbbbbbbbb");
    }

    #[test]
    fn checksum_detects_corruption() {
        let mut page = SlottedPage::init(1, PageType::SlottedData);
        page.update_checksum();
        page.buf[100] ^= 0xFF;
        assert!(!page.verify_checksum());
    }

    #[test]
    fn insert_at_maintains_order() {
        let mut page = SlottedPage::init(1, PageType::SlottedData);
        page.insert_at(0, b"bbb").unwrap();
        page.insert_at(0, b"aaa").unwrap();
        page.insert_at(2, b"ccc").unwrap();
        assert_eq!(page.read(0).unwrap(), b"aaa");
        assert_eq!(page.read(1).unwrap(), b"bbb");
        assert_eq!(page.read(2).unwrap(), b"ccc");
    }

    #[test]
    fn insert_at_after_delete_and_compact() {
        let mut page = SlottedPage::init(1, PageType::SlottedData);
        page.insert(b"aaa").unwrap();
        page.delete(0);
        page.compact();
        page.insert_at(0, b"bbb").unwrap();
        assert_eq!(page.read(0).unwrap(), b"bbb");
        assert_eq!(page.header().slot_count, 1);
    }
}
