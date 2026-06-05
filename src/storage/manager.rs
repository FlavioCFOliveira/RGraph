use crate::buffer::pool::BufferPool;
use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::bitmap::{BitmapPage, FreeListCache, PAGES_PER_BITMAP};
use crate::storage::meta::{Superblock, encode_superblock};
use crate::storage::page::{PAGE_SIZE, PageId};
use crate::wal::doublewrite::DoubleWriteBuffer;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

// ── Layout constants ──────────────────────────────────────────────────────────

/// Physical page id of the primary superblock copy.
pub const PRIMARY_SB_PAGE_ID: PageId = 0;
/// Physical page id of the mirror superblock copy.
pub const MIRROR_SB_PAGE_ID: PageId = 1;
/// Physical page id of the first bitmap page.
pub const FIRST_BITMAP_PAGE_ID: PageId = 2;
/// Physical page id of the first data page.
pub const FIRST_DATA_PAGE_ID: PageId = 3;
/// Minimum file size: must contain primary SB, mirror SB, and one bitmap page.
pub const MIN_FILE_SIZE: u64 = FIRST_DATA_PAGE_ID * PAGE_SIZE as u64;

// ── Helper ────────────────────────────────────────────────────────────────────

/// Compute how many bitmap pages are needed to cover `next_free_page_id`.
pub fn num_bitmap_pages(next_free_page_id: u64) -> usize {
    if next_free_page_id == 0 {
        return 1;
    }
    (next_free_page_id as usize).div_ceil(PAGES_PER_BITMAP)
}

// ── PageManager ───────────────────────────────────────────────────────────────

/// Simple page manager that owns the meta page, bitmap pages, and an
/// in-memory free-list cache.
pub struct PageManager {
    /// Path to the data file (all pages are stored here).
    pub data_path: PathBuf,
    /// In-memory copy of the superblock.
    pub superblock: Superblock,
    /// Chain of bitmap pages; `bitmaps[i]` covers slot `i`.
    pub bitmaps: Vec<BitmapPage>,
    /// Cached free pages ready to allocate.
    pub free_cache: FreeListCache,
    /// Long-lived handle to the data file.
    data_handle: Option<Box<dyn crate::io::FileHandle>>,
    /// Weak reference to the buffer pool.
    ///
    /// When set, `read_page`/`write_page` route through the pool for cache
    /// coherence, and `free_page` invalidates any resident frame.  The weak
    /// reference avoids a reference cycle between `PageManager` and `BufferPool`
    /// (the pool is owned by the engine, which also owns the page manager).
    pool: Weak<BufferPool>,
    /// Optional double-write buffer for torn-page protection on the direct
    /// (no-pool) write path.  When a pool is attached, the pool owns the DW
    /// buffer and handles staging in its flush path; this field is only used
    /// when writing directly to disk without the pool.
    doublewrite: Option<Arc<DoubleWriteBuffer>>,
}

impl std::fmt::Debug for PageManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageManager")
            .field("data_path", &self.data_path)
            .field("superblock", &self.superblock)
            .field("bitmaps", &self.bitmaps)
            .field("free_cache", &self.free_cache)
            .field("pool", &self.pool.strong_count())
            .finish()
    }
}

impl PageManager {
    pub const DATA_FILE: &str = "rgraph.db";
    pub const CACHE_BATCH: usize = 64;

    /// Initialise a brand-new page manager.
    ///
    /// Creates the data file, pre-allocates space for the three fixed
    /// metadata pages (primary SB, mirror SB, bitmap), and issues
    /// `sync_all`.
    pub fn init(data_path: PathBuf, page_size: u32, fs: &dyn FileSystem) -> io::Result<Self> {
        let mut sb = Superblock::new(page_size);
        // 3 fixed pages: primary SB (0), mirror SB (1), bitmap (2).
        sb.total_page_count = FIRST_DATA_PAGE_ID;
        sb.free_page_count = 0;
        sb.next_free_page_id = FIRST_DATA_PAGE_ID;
        sb.update_checksum();

        // Slot 0 bitmap lives at page FIRST_BITMAP_PAGE_ID.
        let mut bitmap = BitmapPage::new(FIRST_BITMAP_PAGE_ID, 0);
        bitmap.allocate(PRIMARY_SB_PAGE_ID); // superblock primary
        bitmap.allocate(MIRROR_SB_PAGE_ID); // superblock mirror
        bitmap.allocate(FIRST_BITMAP_PAGE_ID); // bitmap page itself
        bitmap.page.update_checksum();

        let handle = fs.open(&data_path, true)?;
        let required_len = MIN_FILE_SIZE;
        let current_len = handle.len().unwrap_or(0);
        if current_len < required_len {
            handle.set_len(required_len)?;
            handle.sync_all()?;
        }

        Ok(Self {
            data_path,
            superblock: sb,
            bitmaps: vec![bitmap],
            free_cache: FreeListCache::new(Self::CACHE_BATCH),
            data_handle: Some(handle),
            pool: Weak::new(),
            doublewrite: None,
        })
    }

    /// Open an existing page manager from a single bitmap buffer.
    ///
    /// This is a convenience wrapper around [`open_multi`] for databases
    /// that fit within one bitmap page (≤ ~508 MiB).
    pub fn open(
        data_path: PathBuf,
        sb: Superblock,
        bitmap_buf: AlignedBuffer,
        fs: &dyn FileSystem,
    ) -> io::Result<Self> {
        Self::open_multi(data_path, sb, vec![bitmap_buf], fs)
    }

    /// Open an existing page manager from multiple bitmap buffers.
    ///
    /// Each buffer in `bitmap_bufs` corresponds to one bitmap slot (slot 0
    /// at index 0, slot 1 at index 1, …).  The caller is responsible for
    /// reading the right number of pages from disk before calling this.
    ///
    /// Use [`num_bitmap_pages`] to compute how many buffers are needed.
    pub fn open_multi(
        data_path: PathBuf,
        sb: Superblock,
        bitmap_bufs: Vec<AlignedBuffer>,
        fs: &dyn FileSystem,
    ) -> io::Result<Self> {
        let bitmaps: Vec<BitmapPage> = bitmap_bufs
            .into_iter()
            .enumerate()
            .map(|(idx, buf)| BitmapPage::from_buf(buf, idx as u64))
            .collect();

        let handle = fs.open(&data_path, false)?;
        let mut pm = Self {
            data_path,
            superblock: sb,
            bitmaps,
            free_cache: FreeListCache::new(Self::CACHE_BATCH),
            data_handle: Some(handle),
            pool: Weak::new(),
            doublewrite: None,
        };
        pm.rebuild_cache();
        Ok(pm)
    }

    /// Attach a buffer pool so that subsequent `read_page`/`write_page`
    /// calls are routed through the pool's cache, and `free_page` invalidates
    /// resident frames.
    ///
    /// The page manager stores only a [`Weak`] reference to avoid a reference
    /// cycle with the engine that owns both objects.
    pub fn set_pool(&mut self, pool: &Arc<BufferPool>) {
        self.pool = Arc::downgrade(pool);
    }

    /// Upgrade the weak buffer-pool reference, returning `Some(Arc<BufferPool>)`
    /// if a pool is currently attached and alive, or `None` otherwise.
    pub fn buffer_pool(&self) -> Option<Arc<BufferPool>> {
        self.pool.upgrade()
    }

    /// Attach a [`DoubleWriteBuffer`] for torn-page protection on the direct
    /// (no-pool) write path.
    ///
    /// When a buffer pool is in use the pool's own DW staging handles
    /// protection; this setter is relevant for the pool-less path only.
    pub fn set_doublewrite(&mut self, dw: Arc<DoubleWriteBuffer>) {
        self.doublewrite = Some(dw);
    }

    /// Run torn-page recovery from the doublewrite buffer.
    ///
    /// Must be called on every open before any write is issued to ensure that
    /// partially-written pages from the previous session are repaired.
    /// Returns the number of pages restored.
    pub fn recover_torn_pages(&self, fs: &dyn FileSystem) -> io::Result<usize> {
        if let Some(dw) = &self.doublewrite {
            dw.recover_torn_pages(&self.data_path, fs)
        } else {
            Ok(0)
        }
    }

    /// Physical page id at which the bitmap for `slot` (0-based region index)
    /// is stored on disk.
    ///
    /// Each region's bitmap lives **inside the region it tracks** — at the
    /// first page of region `slot` (local bit 0), i.e. physical `slot *
    /// PAGES_PER_BITMAP`.  Region 0 is special-cased: its first two pages are
    /// the superblock copies, so its bitmap sits at [`FIRST_BITMAP_PAGE_ID`]
    /// (page 2) instead of page 0.
    ///
    /// This is the fix for finding C7: the previous layout stored slot `S`'s
    /// bitmap at `FIRST_BITMAP_PAGE_ID + S`, which for `S >= 1` aliased live
    /// data pages (slot 1's bitmap landed on data page 3) — silent corruption
    /// once a database grew past one region (~508 MiB).
    pub fn bitmap_physical_page_id(slot: u64) -> PageId {
        if slot == 0 {
            FIRST_BITMAP_PAGE_ID
        } else {
            slot * PAGES_PER_BITMAP as PageId
        }
    }

    /// Is `page_id` a physical bitmap page (reserved, never a data page)?
    ///
    /// True for page 2 (slot 0's bitmap) and for every non-zero multiple of
    /// [`PAGES_PER_BITMAP`] (slot `S >= 1`'s bitmap at `S * PAGES_PER_BITMAP`).
    pub fn is_bitmap_page(page_id: PageId) -> bool {
        page_id == FIRST_BITMAP_PAGE_ID
            || (page_id != 0 && page_id.is_multiple_of(PAGES_PER_BITMAP as PageId))
    }

    /// Ensure the in-memory bitmap chain covers `slot`, lazily creating any
    /// missing slots and reserving each new bitmap's own physical page (plus
    /// the two superblock pages in region 0).
    fn ensure_bitmap(&mut self, slot: u64) {
        while (self.bitmaps.len() as u64) <= slot {
            let new_slot = self.bitmaps.len() as u64;
            let phys = Self::bitmap_physical_page_id(new_slot);
            let mut bmp = BitmapPage::new(phys, new_slot);
            // The bitmap occupies one page inside its own region — reserve it.
            bmp.allocate(phys);
            if new_slot == 0 {
                // Region 0 also holds the primary and mirror superblocks.
                bmp.allocate(PRIMARY_SB_PAGE_ID);
                bmp.allocate(MIRROR_SB_PAGE_ID);
            }
            self.bitmaps.push(bmp);
        }
    }

    /// Allocate a new page id.
    ///
    /// Prioritises the free-list cache (which only ever holds data pages), then
    /// extends from `next_free_page_id`, **skipping** any physical page reserved
    /// for a region bitmap so a bitmap page is never handed out as data and a
    /// data page never overwrites a bitmap (finding C7).
    pub fn allocate_page(&mut self) -> PageId {
        if let Some(pid) = self.free_cache.pop() {
            let bitmap_idx = (pid as usize) / PAGES_PER_BITMAP;
            if bitmap_idx < self.bitmaps.len() {
                self.bitmaps[bitmap_idx].allocate(pid);
            }
            self.superblock.free_page_count = self.superblock.free_page_count.saturating_sub(1);
            return pid;
        }

        loop {
            let pid = self.superblock.next_free_page_id;
            self.superblock.next_free_page_id += 1;
            self.superblock.total_page_count += 1;

            // Make sure the bitmap for this page's region exists (and has its
            // own bitmap page reserved).
            let slot = (pid as usize) / PAGES_PER_BITMAP;
            self.ensure_bitmap(slot as u64);

            if Self::is_bitmap_page(pid) {
                // Reserved bitmap page: mark it allocated and skip it as data.
                self.bitmaps[slot].allocate(pid);
                continue;
            }

            self.bitmaps[slot].allocate(pid);
            return pid;
        }
    }

    /// Reconcile the in-memory allocation state against what is actually on
    /// disk, advancing `next_free_page_id` and marking live pages allocated.
    ///
    /// On a write path that flushes the WAL and the data pages but **not** the
    /// superblock/bitmaps (the server autocommit path never calls `sync()`), a
    /// crash leaves the persisted `next_free_page_id` and the bitmaps lagging
    /// the real high-water mark.  ARIES REDO restores the committed *data*
    /// pages, but not this allocator metadata — so without reconciliation the
    /// allocator would re-hand-out an in-use page and overwrite committed
    /// records, and index rebuild (which scans bitmap-allocated pages) would
    /// miss the recovered data entirely (finding H12).
    ///
    /// This scans every data page present in the file, marks each record-bearing
    /// page allocated in its region bitmap (creating the bitmap slot if needed),
    /// and advances `next_free_page_id` past the highest in-use page.  It only
    /// ever raises `next_free_page_id`, so it is idempotent and safe on a cleanly
    /// synced database (where it re-marks the same pages and changes nothing).
    /// Torn or zeroed placeholder pages (checksum failure or `slot_count == 0`)
    /// are skipped, so a page allocated but not yet written stays reusable.
    pub fn reconcile_from_disk(&mut self, fs: &dyn FileSystem) -> io::Result<()> {
        use crate::storage::page::SlottedPage;

        let handle = fs.open(&self.data_path, false)?;
        let file_len = handle.len()?;
        let high_water = file_len / PAGE_SIZE as u64;

        let mut max_in_use = self.superblock.next_free_page_id.saturating_sub(1);
        let mut pid = FIRST_DATA_PAGE_ID;
        while pid < high_water {
            if Self::is_bitmap_page(pid) {
                pid += 1;
                continue;
            }
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            let offset = pid * PAGE_SIZE as u64;
            if handle.read_at(&mut buf, offset).is_err() || !SlottedPage::verify_checksum_bytes(&buf)
            {
                pid += 1;
                continue;
            }
            let page = SlottedPage::new(buf);
            if page.header().slot_count > 0 {
                let slot = (pid as usize) / PAGES_PER_BITMAP;
                self.ensure_bitmap(slot as u64);
                self.bitmaps[slot].allocate(pid);
                if pid > max_in_use {
                    max_in_use = pid;
                }
            }
            pid += 1;
        }

        let reconciled = max_in_use.saturating_add(1);
        if reconciled > self.superblock.next_free_page_id {
            self.superblock.next_free_page_id = reconciled;
        }
        if high_water > self.superblock.total_page_count {
            self.superblock.total_page_count = high_water;
        }
        Ok(())
    }

    /// Return a page to the free list.
    ///
    /// If a buffer pool has been attached via [`set_pool`], any frame
    /// currently caching `page_id` is invalidated so that a subsequent
    /// reallocation of the same id never serves stale bytes.
    pub fn free_page(&mut self, page_id: PageId) {
        // Never free reserved metadata pages (superblock copies or region
        // bitmaps); doing so would let a later allocation overwrite them.
        if page_id < FIRST_DATA_PAGE_ID || Self::is_bitmap_page(page_id) {
            return;
        }
        let bitmap_idx = (page_id as usize) / PAGES_PER_BITMAP;
        if bitmap_idx < self.bitmaps.len() {
            self.bitmaps[bitmap_idx].free(page_id);
        }
        self.free_cache.push(page_id);
        self.superblock.free_page_count += 1;

        // Invalidate any resident pool frame so a reallocation gets a clean page.
        if let Some(pool) = self.pool.upgrade() {
            pool.invalidate(page_id);
        }
    }

    /// Read a page into `buf`, routing through the buffer pool when available.
    ///
    /// When a pool is attached, the page is fetched via `fix_page` (potentially
    /// a cache hit), its bytes are copied into `buf`, and the guard is dropped
    /// immediately.  When no pool is attached, the page is read directly from
    /// the file handle and its checksum is verified.
    pub fn read_page(
        &self,
        fs: &dyn FileSystem,
        page_id: PageId,
        buf: &mut AlignedBuffer,
    ) -> io::Result<()> {
        if let Some(pool) = self.pool.upgrade() {
            let guard = pool.fix_page(fs, page_id)?;
            buf.copy_from_slice(guard.buf());
            // Guard drop unpins the frame.
            return Ok(());
        }

        // Direct path (no pool).
        let handle = self.data_handle.as_ref().expect("data file not open");
        let offset = page_id * PAGE_SIZE as u64;
        handle.read_at(buf, offset)?;
        if !crate::storage::page::SlottedPage::verify_checksum_bytes(buf) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("page {} checksum mismatch", page_id),
            ));
        }
        Ok(())
    }

    /// Write `buf` to `page_id`, routing through the buffer pool when available.
    ///
    /// When a pool is attached, the frame is fixed in the pool, the caller's
    /// data is copied in, and the frame is marked dirty (the background flusher
    /// handles durability in batches — no per-write `fsync`).  When no pool is
    /// attached, the write goes directly to disk with a `sync_data()` call.
    pub fn write_page(
        &self,
        fs: &dyn FileSystem,
        page_id: PageId,
        buf: &mut AlignedBuffer,
    ) -> io::Result<()> {
        if let Some(pool) = self.pool.upgrade() {
            // Ensure the file is large enough for this page before the pool
            // tries to read it (fix_page reads on miss).
            let handle = self.data_handle.as_ref().expect("data file not open");
            let required_len = page_id * PAGE_SIZE as u64 + PAGE_SIZE as u64;
            let current_len = handle.len()?;
            if current_len < required_len {
                // Write a zeroed placeholder so fix_page can verify its checksum.
                let mut placeholder = crate::storage::page::SlottedPage::init(
                    page_id,
                    crate::storage::page::PageType::SlottedData,
                );
                placeholder.update_checksum();
                handle.set_len(required_len)?;
                handle.sync_all()?;
                handle.write_at(&placeholder.buf, page_id * PAGE_SIZE as u64)?;
                handle.sync_data()?;
            }

            let mut guard = pool.fix_page(fs, page_id)?;
            // Copy caller's data into the pool frame.
            crate::storage::page::SlottedPage::update_checksum_bytes(buf);
            guard.buf_mut().copy_from_slice(buf);
            // Mark dirty — the flusher will persist this in a batched write.
            // LSN u64::MAX means "no WAL record" (WAL-before-data: always flushable).
            guard.set_dirty(u64::MAX);
            // Guard drop unpins the frame.
            return Ok(());
        }

        // Direct path (no pool): update checksum and write synchronously.
        crate::storage::page::SlottedPage::update_checksum_bytes(buf);
        let handle = self.data_handle.as_ref().expect("data file not open");
        let offset = page_id * PAGE_SIZE as u64;
        let required_len = offset + PAGE_SIZE as u64;
        let current_len = handle.len()?;
        if current_len < required_len {
            handle.set_len(required_len)?;
            handle.sync_all()?;
        }

        // Stage through double-write buffer before the final write.
        if let Some(dw) = &self.doublewrite {
            let pages: Vec<(u64, &[u8])> = vec![(page_id, &buf[..])];
            dw.write_batch(&pages, fs)?;
        }

        handle.write_at(buf, offset)?;
        handle.sync_data()?;

        // Clear the DW buffer after the in-place write succeeded.
        if let Some(dw) = &self.doublewrite {
            let _ = dw.clear(fs);
        }

        Ok(())
    }

    /// Persist the current superblock to both copies using a **staggered**
    /// double-write so a crash always leaves at least one self-consistent copy
    /// and the generation tiebreak can pick the newest (finding 211).
    ///
    /// The previous protocol wrote both copies back-to-back at the *same*
    /// generation under a single trailing `sync_all`, so a crash could leave
    /// both copies in flight (both torn) or persist only one with no way to
    /// tell which is newer.  Worse, it bumped a *local* copy's generation, so
    /// the in-memory `generation` never advanced and consecutive syncs reused
    /// the same number.
    ///
    /// The staggered protocol is:
    ///   1. write the **mirror** at `gen + 1`, `sync_all` — the mirror now
    ///      durably holds the new state while the *old* primary (still at the
    ///      previous generation) remains a valid fallback;
    ///   2. write the **primary** at `gen + 2`, `sync_all` — the primary
    ///      becomes the newest copy; if its write is torn, the mirror at
    ///      `gen + 1` is a fully-consistent fallback that still reflects this
    ///      sync's state.
    ///
    /// The two copies are therefore always one generation apart and never both
    /// in flight, and the persisted in-memory `generation` advances monotonically
    /// so the tiebreak ([`load_superblock`] prefers the higher generation, ties
    /// to the primary) is always unambiguous.
    pub fn sync_superblock(&mut self, _fs: &dyn FileSystem) -> io::Result<()> {
        let base = self.superblock.generation;
        let handle = self.data_handle.as_ref().expect("data file not open");

        // 1. Mirror at gen+1, made durable before the primary is touched.
        let mut mirror = self.superblock;
        mirror.generation = base + 1;
        mirror.update_checksum();
        let mirror_enc = encode_superblock(&mirror);
        handle.write_at(&mirror_enc, PAGE_SIZE as u64)?;
        handle.sync_all()?;

        // 2. Primary at gen+2, made durable after the mirror is safe.
        let mut primary = self.superblock;
        primary.generation = base + 2;
        primary.update_checksum();
        let primary_enc = encode_superblock(&primary);
        handle.write_at(&primary_enc, 0)?;
        handle.sync_all()?;

        // Advance the in-memory generation so the next sync stays monotonic.
        self.superblock.generation = base + 2;
        Ok(())
    }

    /// Persist ALL bitmap pages to disk.
    pub fn sync_bitmaps(&mut self, _fs: &dyn FileSystem) -> io::Result<()> {
        let handle = self.data_handle.as_ref().expect("data file not open");
        for bitmap in &mut self.bitmaps {
            let page_id = bitmap.page.header().page_id;
            let buf = &mut bitmap.page.buf;
            crate::storage::page::SlottedPage::update_checksum_bytes(buf);
            let offset = page_id * PAGE_SIZE as u64;
            let required_len = offset + PAGE_SIZE as u64;
            let current_len = handle.len()?;
            if current_len < required_len {
                handle.set_len(required_len)?;
                handle.sync_all()?;
            }
            handle.write_at(buf, offset)?;
        }
        handle.sync_data()
    }

    /// Persist the first bitmap page to disk.
    ///
    /// This is a backward-compatibility alias for [`sync_bitmaps`] that
    /// writes only bitmap slot 0 (the common case for small databases).
    pub fn sync_bitmap(&mut self, _fs: &dyn FileSystem) -> io::Result<()> {
        let handle = self.data_handle.as_ref().expect("data file not open");
        let bitmap = &mut self.bitmaps[0];
        let page_id = bitmap.page.header().page_id;
        let buf = &mut bitmap.page.buf;
        crate::storage::page::SlottedPage::update_checksum_bytes(buf);
        let offset = page_id * PAGE_SIZE as u64;
        let required_len = offset + PAGE_SIZE as u64;
        let current_len = handle.len()?;
        if current_len < required_len {
            handle.set_len(required_len)?;
            handle.sync_all()?;
        }
        handle.write_at(buf, offset)?;
        handle.sync_data()
    }

    /// Rebuild the free-list cache by scanning all bitmap pages.
    fn rebuild_cache(&mut self) {
        self.free_cache.pages.clear();
        for bitmap in &self.bitmaps {
            let base = bitmap.base_page_id();
            for i in 0..PAGES_PER_BITMAP {
                if self.free_cache.pages.len() >= self.free_cache.batch_size {
                    return;
                }
                let pid = base + i as PageId;
                // Never hand out reserved metadata pages (superblock copies or
                // region bitmaps) as free data pages.
                if pid < FIRST_DATA_PAGE_ID || Self::is_bitmap_page(pid) {
                    continue;
                }
                if !bitmap.is_set(pid) {
                    self.free_cache.pages.push(pid);
                }
            }
        }
    }

    /// Return every allocated page id (including metadata pages).
    pub fn allocated_pages(&self) -> Vec<PageId> {
        let mut ids = Vec::new();
        for bitmap in &self.bitmaps {
            ids.extend(bitmap.allocated_pages());
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::fault::{FaultConfig, FaultInjectFileSystem, FaultKind, FaultRule, OpMask};
    use crate::io::posix::PosixFileSystem;

    fn temp_fs() -> (tempfile::TempDir, PosixFileSystem, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PageManager::DATA_FILE);
        let fs = PosixFileSystem::new(false);
        (dir, fs, path)
    }

    #[test]
    fn init_and_allocate() {
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path, PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        let pid = pm.allocate_page();
        assert_eq!(pid, FIRST_DATA_PAGE_ID);
        let pid2 = pm.allocate_page();
        assert_eq!(pid2, FIRST_DATA_PAGE_ID + 1);
    }

    #[test]
    fn free_and_reuse() {
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path, PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        let pid = pm.allocate_page();
        pm.free_page(pid);
        let pid2 = pm.allocate_page();
        assert_eq!(pid, pid2); // reused
    }

    #[test]
    fn write_and_read_roundtrip() {
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        let pid = pm.allocate_page();
        let mut page = crate::storage::page::SlottedPage::init(
            pid,
            crate::storage::page::PageType::SlottedData,
        );
        page.buf[crate::storage::page::SlottedPage::HEADER_SIZE] = 0xAB;
        page.update_checksum();
        pm.write_page(&fs, pid, &mut page.buf).unwrap();

        let mut read_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        pm.read_page(&fs, pid, &mut read_buf).unwrap();
        assert_eq!(
            read_buf[crate::storage::page::SlottedPage::HEADER_SIZE],
            0xAB
        );
    }

    #[test]
    fn read_page_detects_corruption() {
        use crate::storage::page::{PageType, SlottedPage};
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        let pid = pm.allocate_page();
        let mut page = SlottedPage::init(pid, PageType::SlottedData);
        page.update_checksum();
        pm.write_page(&fs, pid, &mut page.buf).unwrap();

        // Corrupt a byte in the data area on disk via a separate handle.
        let handle = fs.open(&path, false).unwrap();
        let corrupt_offset = pid * PAGE_SIZE as u64 + SlottedPage::HEADER_SIZE as u64 + 10;
        handle.write_at(&[0xFF], corrupt_offset).unwrap();
        handle.sync_data().unwrap();

        let mut read_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        let result = pm.read_page(&fs, pid, &mut read_buf);
        assert!(
            result.is_err(),
            "corrupted page should fail checksum verification"
        );
    }

    #[test]
    fn sync_all_after_file_growth() {
        let (_dir, inner_fs, path) = temp_fs();
        let fs = FaultInjectFileSystem::new(Box::new(inner_fs));

        // Init with no faults.
        let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();

        // Configure every sync_all to fail.
        fs.set_config(FaultConfig {
            rules: vec![FaultRule {
                op_mask: OpMask {
                    sync_all: true,
                    ..OpMask::default()
                },
                kind: FaultKind::Eio,
                every_n: None,
            }],
        });

        let pid = pm.allocate_page();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        let result = pm.write_page(&fs, pid, &mut buf);
        assert!(
            result.is_err(),
            "write_page must call sync_all after set_len when growing"
        );
    }

    /// Regression gate for finding C7 (2026-06-05): a region's bitmap page must
    /// live INSIDE its own region, never on top of a data page.  The old layout
    /// placed slot S's bitmap at `FIRST_BITMAP_PAGE_ID + S`, so slot 1's bitmap
    /// landed on data page 3 — silent corruption once the database grew past one
    /// region.  This test fast-forwards the allocator to the region-0/region-1
    /// boundary (avoiding 65 024 real allocations) and asserts the bitmap page is
    /// skipped, self-reserved, and never aliases a data page.
    #[test]
    fn bitmap_boundary_does_not_alias_data_page() {
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path, PAGE_SIZE as u32, &fs).unwrap();

        // Region-0 baseline: the first data page is page 3, not a bitmap page.
        let first = pm.allocate_page();
        assert_eq!(first, FIRST_DATA_PAGE_ID);
        assert!(!PageManager::is_bitmap_page(first));

        // Jump to the boundary without allocating the whole region.
        pm.superblock.next_free_page_id = PAGES_PER_BITMAP as u64;

        // The next allocation must SKIP the region-1 bitmap page (at PAGES_PER_BITMAP)
        // and return the first *data* page of region 1 (PAGES_PER_BITMAP + 1).
        let boundary_alloc = pm.allocate_page();
        assert_eq!(
            boundary_alloc,
            PAGES_PER_BITMAP as u64 + 1,
            "allocator must skip the region-1 bitmap page and return its first data page"
        );

        // A second slot must now exist with its own bitmap page reserved.
        assert_eq!(pm.bitmaps.len(), 2, "region-1 bitmap slot must be created");
        let region1_bitmap = PageManager::bitmap_physical_page_id(1);
        assert_eq!(region1_bitmap, PAGES_PER_BITMAP as u64);
        assert!(
            pm.bitmaps[1].is_set(region1_bitmap),
            "the region-1 bitmap page must be marked allocated within its own slot"
        );

        // Core anti-aliasing invariant: the bitmap page must NOT be page 3 (the
        // old bug), must be classified as a bitmap page, and must never equal a
        // page handed out as data.
        assert_ne!(
            region1_bitmap, FIRST_DATA_PAGE_ID,
            "region-1 bitmap must not alias data page 3"
        );
        assert!(PageManager::is_bitmap_page(region1_bitmap));
        assert_ne!(
            boundary_alloc, region1_bitmap,
            "a data allocation must never alias a bitmap page"
        );
    }

    /// C7 durability path: a region-1 bitmap stored inside region 1 (at
    /// `PAGES_PER_BITMAP`) must be written to and read back from its real
    /// physical offset on reopen, and a region-1 data page must survive without
    /// aliasing region-0 data.  Backed by a sparse file so only the few touched
    /// pages cost disk (the logical size is ~508 MiB).
    #[test]
    fn multi_region_bitmap_survives_reopen() {
        use crate::storage::meta::load_superblock;
        use crate::storage::page::{PageType, SlottedPage};

        let (_dir, fs, path) = temp_fs();

        let (data_pid, region1_bitmap) = {
            let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();

            // A real region-0 data page with a sentinel.
            let p0 = pm.allocate_page();
            let mut page0 = SlottedPage::init(p0, PageType::SlottedData);
            page0.buf[SlottedPage::HEADER_SIZE] = 0x11;
            page0.update_checksum();
            pm.write_page(&fs, p0, &mut page0.buf).unwrap();

            // Cross into region 1; allocate its first data page and write a sentinel.
            pm.superblock.next_free_page_id = PAGES_PER_BITMAP as u64;
            let p1 = pm.allocate_page();
            assert_eq!(p1, PAGES_PER_BITMAP as u64 + 1);
            let mut page1 = SlottedPage::init(p1, PageType::SlottedData);
            page1.buf[SlottedPage::HEADER_SIZE] = 0x22;
            page1.update_checksum();
            pm.write_page(&fs, p1, &mut page1.buf).unwrap();

            pm.sync_superblock(&fs).unwrap();
            pm.sync_bitmaps(&fs).unwrap();
            (p1, PageManager::bitmap_physical_page_id(1))
        };

        // Reopen: read every bitmap slot from its real physical location.
        let sb = load_superblock(&fs, &path).unwrap();
        let count = num_bitmap_pages(sb.next_free_page_id).max(1);
        assert_eq!(count, 2, "next_free_page_id must span two bitmap regions");
        let handle = fs.open(&path, false).unwrap();
        let mut bufs = Vec::new();
        for i in 0..count {
            let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
            let off = PageManager::bitmap_physical_page_id(i as u64) * PAGE_SIZE as u64;
            handle.read_at(&mut buf, off).unwrap();
            bufs.push(buf);
        }
        drop(handle);
        let pm = PageManager::open_multi(path.clone(), sb, bufs, &fs).unwrap();

        // The region-1 bitmap slot must record both its own bitmap page and the
        // data page as allocated.
        assert!(
            pm.bitmaps[1].is_set(region1_bitmap),
            "region-1 bitmap page must read back as allocated"
        );
        assert!(
            pm.bitmaps[1].is_set(data_pid),
            "region-1 data page must read back as allocated"
        );

        // The region-1 data sentinel must survive (no aliasing of region-0 data).
        let mut rb = AlignedBuffer::zeroed(PAGE_SIZE);
        pm.read_page(&fs, data_pid, &mut rb).unwrap();
        assert_eq!(
            rb[SlottedPage::HEADER_SIZE],
            0x22,
            "region-1 data must survive reopen intact"
        );
    }

    /// Finding H12 (Task 210): `reconcile_from_disk` must raise a stale
    /// `next_free_page_id` to the real high-water mark and mark record-bearing
    /// pages allocated, while leaving empty placeholder pages reusable.
    #[test]
    fn reconcile_from_disk_recovers_high_water_mark() {
        use crate::storage::page::{PageType, SlottedPage};
        let (_dir, fs, path) = temp_fs();

        // Write two real data pages with records via a fresh manager.
        let (p_a, p_b) = {
            let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();
            let a = pm.allocate_page();
            let b = pm.allocate_page();
            for pid in [a, b] {
                let mut page = SlottedPage::init(pid, PageType::SlottedData);
                page.insert(&[0xCDu8; 16]); // one slot -> slot_count > 0
                page.update_checksum();
                pm.write_page(&fs, pid, &mut page.buf).unwrap();
            }
            // Deliberately do NOT sync the superblock/bitmaps (crash model).
            (a, b)
        };

        // Reopen with a STALE superblock (next_free_page_id still at the initial
        // FIRST_DATA_PAGE_ID) and an empty bitmap, then reconcile.
        let sb = Superblock::new(PAGE_SIZE as u32);
        let bitmap_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        let mut pm = PageManager::open_multi(path, sb, vec![bitmap_buf], &fs).unwrap();
        assert!(pm.superblock.next_free_page_id <= FIRST_DATA_PAGE_ID);

        pm.reconcile_from_disk(&fs).unwrap();

        // High-water mark advanced past the last record-bearing page.
        assert_eq!(pm.superblock.next_free_page_id, p_b + 1);
        // Both record pages are now marked allocated.
        assert!(pm.bitmaps[0].is_set(p_a));
        assert!(pm.bitmaps[0].is_set(p_b));
        // A fresh allocation must not collide with the recovered pages.
        let fresh = pm.allocate_page();
        assert!(fresh > p_b, "fresh allocation must not alias a recovered page");
    }

    /// Regression gate for finding 211 (Task 211, 2026-06-05): the staggered
    /// superblock double-write must leave the two copies one generation apart
    /// (so the tiebreak is unambiguous), advance the in-memory generation
    /// monotonically, and survive a torn copy on either side — recovering the
    /// latest committed state every time.
    #[test]
    fn staggered_superblock_survives_a_torn_copy() {
        use crate::storage::meta::{decode_superblock, load_superblock};
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();

        // The in-memory generation must advance across syncs (the old code bumped
        // only a local copy, so consecutive syncs reused the same number).
        let g1 = pm.superblock.generation;
        pm.superblock.next_free_page_id = 999;
        pm.sync_superblock(&fs).unwrap();
        assert!(
            pm.superblock.generation > g1,
            "in-memory generation must advance monotonically across syncs"
        );

        // On disk, the primary must be exactly the newer (higher-generation)
        // staggered copy, and both copies must individually validate and reflect
        // the latest state.
        let handle = fs.open(&path, false).unwrap();
        let mut p0 = AlignedBuffer::zeroed(PAGE_SIZE);
        let mut p1 = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut p0, 0).unwrap();
        handle.read_at(&mut p1, PAGE_SIZE as u64).unwrap();
        let primary = decode_superblock(&p0).expect("primary copy must be self-consistent");
        let mirror = decode_superblock(&p1).expect("mirror copy must be self-consistent");
        assert_eq!(primary.next_free_page_id, 999);
        assert_eq!(mirror.next_free_page_id, 999);
        assert!(
            primary.generation > mirror.generation,
            "primary must be one generation ahead of the mirror (staggered), got primary={} mirror={}",
            primary.generation,
            mirror.generation
        );

        // Crash that tore the PRIMARY copy: the durable mirror still recovers the
        // latest committed state.
        let garbage = vec![0xA5u8; PAGE_SIZE];
        handle.write_at(&garbage, 0).unwrap();
        handle.sync_data().unwrap();
        let recovered = load_superblock(&fs, &path).unwrap();
        assert_eq!(
            recovered.next_free_page_id, 999,
            "a torn primary must still recover the latest state from the mirror"
        );
        assert!(recovered.verify_checksum());

        // Crash that tore the MIRROR copy instead: re-persist a clean pair, then
        // corrupt page 1 — the higher-generation primary recovers the state.
        pm.sync_superblock(&fs).unwrap();
        handle.write_at(&garbage, PAGE_SIZE as u64).unwrap();
        handle.sync_data().unwrap();
        drop(handle);
        let recovered2 = load_superblock(&fs, &path).unwrap();
        assert_eq!(recovered2.next_free_page_id, 999);
        assert!(recovered2.verify_checksum());
    }

    #[test]
    fn num_bitmap_pages_calculation() {
        assert_eq!(num_bitmap_pages(0), 1);
        assert_eq!(num_bitmap_pages(1), 1);
        assert_eq!(num_bitmap_pages(PAGES_PER_BITMAP as u64), 1);
        assert_eq!(num_bitmap_pages(PAGES_PER_BITMAP as u64 + 1), 2);
        assert_eq!(num_bitmap_pages(2 * PAGES_PER_BITMAP as u64), 2);
        assert_eq!(num_bitmap_pages(2 * PAGES_PER_BITMAP as u64 + 1), 3);
    }

    #[test]
    fn sync_bitmaps_writes_all_slots() {
        let (_dir, fs, path) = temp_fs();
        let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmaps(&fs).unwrap();

        // Allocate a page and persist.
        let pid = pm.allocate_page();
        assert!(pm.bitmaps[0].is_set(pid));
        pm.sync_bitmaps(&fs).unwrap();
    }

    #[test]
    fn open_multi_restores_bitmap_chain() {
        let (_dir, fs, path) = temp_fs();

        // Init, allocate some pages, sync.
        let allocated_pid = {
            let mut pm = PageManager::init(path.clone(), PAGE_SIZE as u32, &fs).unwrap();
            pm.sync_superblock(&fs).unwrap();
            pm.sync_bitmap(&fs).unwrap();
            let pid = pm.allocate_page();
            pm.sync_superblock(&fs).unwrap();
            pm.sync_bitmap(&fs).unwrap();
            pid
        };

        // Reopen using open_multi with one bitmap buffer.
        use crate::storage::meta::decode_superblock;
        let handle = fs.open(&path, false).unwrap();
        let mut sb_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut sb_buf, 0).unwrap();
        let sb = decode_superblock(&sb_buf).unwrap();
        let mut bitmap_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle
            .read_at(
                &mut bitmap_buf,
                FIRST_BITMAP_PAGE_ID * PAGE_SIZE as u64,
            )
            .unwrap();
        drop(handle);

        let pm = PageManager::open_multi(path, sb, vec![bitmap_buf], &fs).unwrap();
        assert!(
            pm.bitmaps[0].is_set(allocated_pid),
            "previously allocated page must be set in restored bitmap"
        );
    }

    /// Task 152 acceptance criterion: torn-write fault injection.
    ///
    /// Scenario:
    ///   1. Initialise a `PageManager` with a `DoubleWriteBuffer` attached.
    ///   2. Write a page with known data — the DW write and in-place write both
    ///      succeed normally.
    ///   3. Simulate a torn write by manually corrupting the in-place location
    ///      on disk (as if the machine crashed mid-write).
    ///   4. Call `recover_torn_pages()`.
    ///   5. Verify the in-place copy has been restored from the DW image.
    #[test]
    fn doublewrite_recover_torn_page() {
        use crate::io::posix::PosixFileSystem;
        use crate::storage::page::{PAGE_SIZE, PageType, SlottedPage};
        use crate::wal::doublewrite::DoubleWriteBuffer;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let data_path = dir.path().join(PageManager::DATA_FILE);
        let dw_path = dir.path().join("test.dw");

        // Initialise the page manager and attach a DW buffer.
        let mut pm = PageManager::init(data_path.clone(), PAGE_SIZE as u32, &fs).unwrap();
        pm.sync_superblock(&fs).unwrap();
        pm.sync_bitmap(&fs).unwrap();
        let dw = Arc::new(DoubleWriteBuffer::open(dw_path, &fs).unwrap());
        pm.set_doublewrite(dw.clone());

        // Allocate a page and write a known sentinel.
        let pid = pm.allocate_page();
        let mut page = SlottedPage::init(pid, PageType::SlottedData);
        page.buf[SlottedPage::HEADER_SIZE] = 0xDE;
        page.buf[SlottedPage::HEADER_SIZE + 1] = 0xAD;
        page.update_checksum();
        pm.write_page(&fs, pid, &mut page.buf).unwrap();

        // At this point the DW buffer was cleared by write_page (normal path).
        // Re-stage the page manually to simulate a state where the DW buffer
        // holds a good copy but the in-place location is corrupt (torn write).
        let pages: Vec<(u64, &[u8])> = vec![(pid, &page.buf[..])];
        dw.write_batch(&pages, &fs).unwrap();

        // Corrupt the in-place page on disk (simulate a torn write).
        let handle = fs.open(&data_path, false).unwrap();
        let corrupt_offset = pid * PAGE_SIZE as u64;
        let corrupt_buf = vec![0xFFu8; PAGE_SIZE];
        handle.write_at(&corrupt_buf, corrupt_offset).unwrap();
        handle.sync_data().unwrap();
        drop(handle);

        // Verify that the in-place copy is now bad.
        let handle = fs.open(&data_path, false).unwrap();
        let mut check_buf = crate::io::AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut check_buf, corrupt_offset).unwrap();
        drop(handle);
        assert!(
            !SlottedPage::verify_checksum_bytes(&check_buf),
            "the in-place copy must be corrupt before recovery"
        );

        // Run torn-page recovery.
        let restored = pm.recover_torn_pages(&fs).unwrap();
        assert_eq!(restored, 1, "exactly one torn page must be restored");

        // Verify the in-place copy matches the original good page.
        let handle = fs.open(&data_path, false).unwrap();
        let mut recovered_buf = crate::io::AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut recovered_buf, corrupt_offset).unwrap();
        drop(handle);
        assert_eq!(
            &recovered_buf[..],
            &page.buf[..],
            "restored page must match the original doublewrite copy"
        );
        assert!(
            SlottedPage::verify_checksum_bytes(&recovered_buf),
            "restored page must have a valid checksum"
        );
    }
}
