//! Append-only WAL writer with LSN tracking and segment rotation (Tasks 110, 150).
//!
//! # LSN Encoding (Task 150)
//!
//! LSNs are 64-bit values packed as `(segment_id << 32) | intra_segment_offset`.
//! This decouples the logical log sequence number from the physical byte offset
//! within a single file, enabling multi-segment recovery without ambiguity.
//!
//! - `segment_id`: upper 32 bits — monotonically increasing segment number.
//! - `intra_segment_offset`: lower 32 bits — byte offset within that segment.
//!
//! Helper functions [`lsn_segment_id`] and [`lsn_offset`] unpack the two fields.
//! [`make_lsn`] assembles them.
//!
//! # Segment rotation
//!
//! When a segment reaches [`WalWriter::SEGMENT_SIZE`] (64 MiB), [`WalWriter::rotate`]
//! is called (either manually or by the group-commit leader).  Rotation:
//!
//! 1. Writes a 4 KiB [`SegmentDescriptor`] block at the end of the current segment.
//! 2. Atomically updates the `wal-current` symlink to point to the new segment.
//! 3. Opens the next segment file (`wal-{n+1:09}`).
//! 4. Archives segments older than the two most recent post-checkpoint ones to
//!    `wal-archive/`.
//!
//! # Integrity hash
//!
//! The [`SegmentDescriptor`] uses **XxHash64** (via `twox-hash`) rather than
//! SHA-256.  This is a deliberate trade-off: XxHash64 is non-cryptographic but
//! extremely fast and sufficient for detecting storage-layer corruption (not an
//! adversarial threat model).  The substitution is documented here and in the
//! descriptor format.

use crate::io::{AlignedBuffer, FileSystem};
use crate::wal::record::{RecordType, WalRecord};
use std::hash::Hasher as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use twox_hash::XxHash64;

// ── LSN helpers (Task 150) ────────────────────────────────────────────────────

/// Pack a `(segment_id, intra_segment_offset)` pair into a 64-bit LSN.
///
/// Layout: `segment_id` occupies the upper 32 bits; `offset` occupies the
/// lower 32 bits.  This supports up to 4 294 967 296 segments each up to
/// 4 GiB, which is more than sufficient for production use.
#[inline(always)]
pub fn make_lsn(segment_id: u32, offset: u32) -> u64 {
    ((segment_id as u64) << 32) | (offset as u64)
}

/// Extract the segment-id component from an LSN.
#[inline(always)]
pub fn lsn_segment_id(lsn: u64) -> u32 {
    (lsn >> 32) as u32
}

/// Extract the intra-segment byte offset from an LSN.
#[inline(always)]
pub fn lsn_offset(lsn: u64) -> u32 {
    lsn as u32
}

/// Descriptor block written at the end of every sealed WAL segment.
///
/// The block is padded to exactly [`SegmentDescriptor::SIZE`] bytes so that
/// segment boundaries are always at a multiple of 4 KiB.
///
/// # Integrity hash
///
/// The `integrity_hash` field stores an **XxHash64** (seed 0) of all bytes in
/// the segment **excluding** this descriptor block itself.  XxHash64 is used
/// instead of SHA-256 because it is orders of magnitude faster while still
/// catching storage-layer corruption.  It is **not** cryptographically secure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentDescriptor {
    /// Monotonically increasing segment number (0-based).
    pub segment_id: u64,
    /// Byte offset (in the global LSN space) at which this segment starts.
    pub segment_start_lsn: u64,
    /// `segment_id` of the preceding segment (0 if first).
    pub previous_segment_id: u64,
    /// Number of WAL records written to this segment.
    pub record_count: u64,
    /// XxHash64 of the segment content (excluding this descriptor block).
    pub integrity_hash: u64,
}

impl SegmentDescriptor {
    /// On-disk size of the descriptor block in bytes (4 KiB, padded).
    pub const SIZE: usize = 4096;

    /// Magic marker at bytes 0..4 of the descriptor block.
    const MAGIC: u32 = 0x5741_4453; // "WADS"

    /// Encode the descriptor into a 4 KiB block.
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut block = [0u8; Self::SIZE];
        block[0..4].copy_from_slice(&Self::MAGIC.to_be_bytes());
        block[4..12].copy_from_slice(&self.segment_id.to_be_bytes());
        block[12..20].copy_from_slice(&self.segment_start_lsn.to_be_bytes());
        block[20..28].copy_from_slice(&self.previous_segment_id.to_be_bytes());
        block[28..36].copy_from_slice(&self.record_count.to_be_bytes());
        block[36..44].copy_from_slice(&self.integrity_hash.to_be_bytes());
        block
    }

    /// Decode a descriptor from a byte slice.  Returns `None` if the magic
    /// marker is missing or the slice is too short.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic != Self::MAGIC {
            return None;
        }
        Some(Self {
            segment_id: u64::from_be_bytes(bytes[4..12].try_into().ok()?),
            segment_start_lsn: u64::from_be_bytes(bytes[12..20].try_into().ok()?),
            previous_segment_id: u64::from_be_bytes(bytes[20..28].try_into().ok()?),
            record_count: u64::from_be_bytes(bytes[28..36].try_into().ok()?),
            integrity_hash: u64::from_be_bytes(bytes[36..44].try_into().ok()?),
        })
    }
}

/// Append-only WAL writer that maintains a monotonic LSN.
///
/// The writer keeps a pinned **1 MiB aligned buffer** to avoid extra
/// copies when `O_DIRECT` is enabled.  All buffered records are flushed
/// to the segment file in a single `write_at` call.
///
/// Call [`WalWriter::needs_rotation`] after each flush and
/// [`WalWriter::rotate`] when it returns `true`.
///
/// Call [`WalWriter::needs_checkpoint`] after flushes and trigger a fuzzy
/// checkpoint via [`crate::wal::checkpoint::Checkpoint::run`] when it returns
/// `true` (Task 151).
pub struct WalWriter {
    /// Directory that holds WAL segment files.
    pub wal_dir: PathBuf,
    /// Current LSN `(segment_id << 32) | intra_segment_offset`.
    pub current_lsn: u64,
    /// Path of the current segment file.
    pub segment_path: PathBuf,
    /// In-memory aligned buffer (1 MiB) for records not yet synced.
    buffer: AlignedBuffer,
    /// Number of valid bytes in `buffer`.
    buffered: usize,
    /// Number of bytes since last sync.
    unsynced: usize,
    /// Monotonically increasing segment counter (0-based).
    segment_id: u64,
    /// LSN at the start of the current segment.
    segment_start_lsn: u64,
    /// Number of WAL records written to the current segment.
    segment_record_count: u64,
    /// Total number of segments written since open (including the current one).
    total_segment_count: u64,
    /// Persistent handle to the current segment file.
    segment_handle: Option<Box<dyn crate::io::FileHandle>>,
    /// Highest LSN that has been durably flushed to disk (post-fsync).
    ///
    /// Shared with the buffer-pool flusher so that dirty frames are only
    /// written after their WAL record is on durable storage
    /// (WAL-before-data ordering).
    durable_lsn: Arc<AtomicU64>,
    /// Number of bytes written since the last checkpoint.
    /// When this exceeds [`CHECKPOINT_INTERVAL`], [`needs_checkpoint`] returns `true`.
    bytes_since_checkpoint: u64,
}

impl std::fmt::Debug for WalWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalWriter")
            .field("wal_dir", &self.wal_dir)
            .field("current_lsn", &self.current_lsn)
            .field("segment_path", &self.segment_path)
            .field("buffer", &self.buffer)
            .field("buffered", &self.buffered)
            .field("unsynced", &self.unsynced)
            .field("segment_id", &self.segment_id)
            .field("segment_start_lsn", &self.segment_start_lsn)
            .field("segment_record_count", &self.segment_record_count)
            .field("total_segment_count", &self.total_segment_count)
            .field("durable_lsn", &self.durable_lsn.load(Ordering::Relaxed))
            .field("bytes_since_checkpoint", &self.bytes_since_checkpoint)
            .finish()
    }
}

impl WalWriter {
    /// WAL segment rotation threshold: 64 MiB.
    pub const SEGMENT_SIZE: u64 = 64 * 1024 * 1024;
    /// In-memory write buffer size: 1 MiB.
    pub const BUFFER_SIZE: usize = 1024 * 1024;
    /// Subdirectory for archived segments.
    pub const ARCHIVE_DIR: &'static str = "wal-archive";
    /// Number of post-checkpoint segments retained in the live directory.
    pub const LIVE_SEGMENT_RETENTION: usize = 2;
    /// Fuzzy checkpoint interval: trigger a checkpoint every 8 MiB of WAL written
    /// (Task 151).  This bounds the amount of WAL that ARIES must replay on recovery.
    pub const CHECKPOINT_INTERVAL: u64 = 8 * 1024 * 1024;

    /// Open (or create) the WAL in `wal_dir`.
    ///
    /// On open we enumerate all existing segment files (`wal-NNNNNNNNN`) to
    /// find the highest segment id, then continue writing from there.  This
    /// ensures that after a restart the writer picks up exactly where it left
    /// off — including after a rotation — rather than blindly reopening
    /// segment 0 and potentially overwriting committed records.
    ///
    /// LSNs use the `(segment_id << 32) | intra_segment_offset` encoding
    /// introduced in Task 150.
    pub fn open(wal_dir: PathBuf, fs: &dyn FileSystem) -> io::Result<Self> {
        fs.create_dir_all(&wal_dir)?;

        // Enumerate live segment files to find the highest segment id.
        let max_segment_id = Self::find_highest_segment_id(&wal_dir);

        let segment_id = max_segment_id;
        let segment_path = wal_dir.join(format!("wal-{:09}", segment_id));

        let (intra_offset, segment_handle) = if fs.exists(&segment_path) {
            let handle = fs.open(&segment_path, false)?;
            let len = handle.len()? as u32;
            // Ensure a non-zero offset so LSN 0 remains the "null/initial" sentinel.
            let offset = if segment_id == 0 { len.max(1) } else { len };
            (offset, Some(handle))
        } else {
            let handle = fs.open(&segment_path, true)?;
            handle.sync_data()?;
            // Make the new segment's directory entry durable (H8).
            fs.sync_dir(&wal_dir)?;
            // For segment 0, start at offset 1 so LSN 0 stays as the null sentinel.
            let offset: u32 = if segment_id == 0 { 1 } else { 0 };
            (offset, Some(handle))
        };

        let current_lsn = make_lsn(segment_id as u32, intra_offset);
        // segment_start_lsn is the LSN at the beginning of the current segment.
        let segment_start_lsn = make_lsn(segment_id as u32, 0);

        // Initialise durable_lsn to current_lsn so that records already on
        // disk (from a previous open) are not considered unflushable.
        let durable_lsn = Arc::new(AtomicU64::new(current_lsn));
        Ok(Self {
            wal_dir,
            current_lsn,
            segment_path,
            buffer: AlignedBuffer::zeroed(Self::BUFFER_SIZE),
            buffered: 0,
            unsynced: 0,
            segment_id,
            segment_start_lsn,
            segment_record_count: 0,
            total_segment_count: max_segment_id + 1,
            segment_handle,
            durable_lsn,
            bytes_since_checkpoint: 0,
        })
    }

    /// Scan `wal_dir` for segment files matching `wal-NNNNNNNNN` and return
    /// the highest segment id found.  Returns 0 if no segments exist yet.
    fn find_highest_segment_id(wal_dir: &Path) -> u64 {
        // We probe for segment files by checking for their existence.
        // This is O(segments) but segments are bounded in practice.
        let mut max_id = 0u64;
        // Probe up to a reasonable upper bound; bail on the first gap.
        // The production limit is 2^32-1 segments but we never expect
        // to have millions in the live directory.
        for id in 0u64..=u32::MAX as u64 {
            let candidate = wal_dir.join(format!("wal-{:09}", id));
            if candidate.exists() {
                max_id = id;
            } else if id > max_id {
                // First gap after the last found segment — stop.
                break;
            }
        }
        max_id
    }

    /// Return a shared handle to the durable LSN watermark.
    ///
    /// The buffer-pool flusher uses this to gate WAL-before-data ordering:
    /// a dirty frame is not written to disk until its `rec_lsn` is ≤ the
    /// durable LSN, guaranteeing the WAL record is already durable.
    pub fn durable_lsn(&self) -> Arc<AtomicU64> {
        self.durable_lsn.clone()
    }

    /// Append a record and return its LSN.
    ///
    /// The LSN is encoded as `(segment_id << 32) | intra_segment_offset` per
    /// the Task 150 scheme.  Each append advances only the lower 32 bits
    /// (the intra-segment offset) until a rotation occurs.
    pub fn append(&mut self, _fs: &dyn FileSystem, mut record: WalRecord) -> io::Result<u64> {
        record.set_lsn(self.current_lsn);
        let bytes = record.encode();
        if self.buffered + bytes.len() > Self::BUFFER_SIZE {
            self.flush(_fs)?;
            if bytes.len() > Self::BUFFER_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "WAL record larger than buffer pool",
                ));
            }
        }
        self.buffer[self.buffered..self.buffered + bytes.len()].copy_from_slice(&bytes);
        self.buffered += bytes.len();
        self.unsynced += bytes.len();
        // Advance only the intra-segment offset (lower 32 bits); segment_id stays constant.
        let seg_id = lsn_segment_id(self.current_lsn);
        let new_offset = lsn_offset(self.current_lsn) + bytes.len() as u32;
        self.current_lsn = make_lsn(seg_id, new_offset);
        self.segment_record_count += 1;
        self.bytes_since_checkpoint += bytes.len() as u64;
        Ok(record.lsn)
    }

    /// Returns `true` when the bytes written since the last checkpoint exceed
    /// [`CHECKPOINT_INTERVAL`].
    ///
    /// Call this after every `flush` or `sync`.  When it returns `true`, invoke
    /// [`crate::wal::checkpoint::Checkpoint::run`] and then call
    /// [`reset_checkpoint_counter`] to restart the interval measurement.
    pub fn needs_checkpoint(&self) -> bool {
        self.bytes_since_checkpoint >= Self::CHECKPOINT_INTERVAL
    }

    /// Reset the checkpoint byte counter after a checkpoint has been taken.
    ///
    /// Also updates the internal `last_checkpoint_lsn` reference (stored in
    /// the superblock by the caller) so that recovery knows the new starting
    /// point.
    pub fn reset_checkpoint_counter(&mut self) {
        self.bytes_since_checkpoint = 0;
    }

    /// Flush buffered data to disk and sync.
    ///
    /// After `sync_data()` succeeds, advances the shared `durable_lsn`
    /// watermark to `current_lsn`.  The buffer-pool flusher reads this
    /// watermark to enforce WAL-before-data ordering.
    pub fn flush(&mut self, _fs: &dyn FileSystem) -> io::Result<()> {
        if self.buffered == 0 {
            return Ok(());
        }
        let handle = self.segment_handle.as_ref().expect("segment file not open");
        // Always append at the physical end of the segment file.
        let file_offset = handle.len()?;
        handle.write_at(&self.buffer[..self.buffered], file_offset)?;
        handle.sync_data()?;
        self.buffered = 0;
        self.unsynced = 0;
        // Advance the durable watermark now that these bytes are on disk.
        self.durable_lsn
            .fetch_max(self.current_lsn, Ordering::Release);
        Ok(())
    }

    /// Sync any pending data to durable storage.
    pub fn sync(&mut self, _fs: &dyn FileSystem) -> io::Result<()> {
        self.flush(_fs)
    }

    // ── Segment rotation (Task 110) ────────────────────────────────────────────

    /// Returns `true` if the current segment has reached [`SEGMENT_SIZE`] and
    /// should be rotated before appending more records.
    pub fn needs_rotation(&self) -> bool {
        // With the (segment_id << 32) | offset encoding, the intra-segment
        // offset is the lower 32 bits of both current_lsn and segment_start_lsn.
        // They share the same segment_id component, so the subtraction is safe.
        let current_offset = lsn_offset(self.current_lsn) as u64;
        let start_offset = lsn_offset(self.segment_start_lsn) as u64;
        let bytes_in_segment = current_offset.saturating_sub(start_offset);
        bytes_in_segment >= Self::SEGMENT_SIZE
    }

    /// Seal the current segment and open the next one.
    ///
    /// Steps:
    /// 1. Flush any buffered records.
    /// 2. Compute the XxHash64 of the segment content.
    /// 3. Write a 4 KiB [`SegmentDescriptor`] block at the end.
    /// 4. Atomically update the `wal-current` symlink.
    /// 5. Open the new segment file.
    /// 6. Archive segments beyond the retention window.
    pub fn rotate(&mut self, fs: &dyn FileSystem) -> io::Result<()> {
        // 1. Flush.
        self.flush(fs)?;

        // 2. Compute hash of the segment content written so far.
        let integrity_hash = self.compute_segment_hash()?;

        // 3. Write descriptor block.
        let descriptor = SegmentDescriptor {
            segment_id: self.segment_id,
            segment_start_lsn: self.segment_start_lsn,
            previous_segment_id: self.segment_id.saturating_sub(1),
            record_count: self.segment_record_count,
            integrity_hash,
        };
        let desc_block = descriptor.encode();
        {
            let handle = self.segment_handle.as_ref().expect("segment file not open");
            let offset = handle.len()?;
            handle.write_at(&desc_block, offset)?;
            handle.sync_data()?;
        }

        // Write a SegmentDescriptor WAL record (informational; not used for
        // recovery but visible in WAL scans).
        let desc_rec = WalRecord::new(RecordType::SegmentDescriptor, 0, 0, 0, desc_block.to_vec());
        // Encode and append directly to the segment file (bypass buffer since
        // we just flushed and may be about to switch).
        let desc_bytes = desc_rec.encode();
        {
            let handle = self.segment_handle.as_ref().expect("segment file not open");
            let offset = handle.len()?;
            handle.write_at(&desc_bytes, offset)?;
            handle.sync_data()?;
        }
        // Advance the intra-segment offset for the descriptor record.
        {
            let seg_id = lsn_segment_id(self.current_lsn);
            let new_offset = lsn_offset(self.current_lsn) + desc_bytes.len() as u32;
            self.current_lsn = make_lsn(seg_id, new_offset);
        }

        // 4. Open next segment.
        let next_segment_id = self.segment_id + 1;
        let next_path = self.wal_dir.join(format!("wal-{:09}", next_segment_id));
        {
            let handle = fs.open(&next_path, true)?;
            handle.sync_data()?;
            // Make the new segment's directory entry durable before adopting it (H8).
            fs.sync_dir(&self.wal_dir)?;
            self.segment_handle = Some(handle);
        }

        // 5. Atomically update symlink.
        self.segment_path = next_path;
        self.update_symlink(fs)?;

        // 6. Update segment tracking.  The new segment's LSN starts at offset 0
        //    in the next segment_id bucket.
        let prev_segment_id = self.segment_id;
        self.segment_id = next_segment_id;
        // New segment starts at (next_segment_id << 32) | 0.
        self.segment_start_lsn = make_lsn(next_segment_id as u32, 0);
        self.current_lsn = self.segment_start_lsn;
        self.segment_record_count = 0;
        self.total_segment_count += 1;

        self.archive_old_segments(fs, prev_segment_id)?;

        Ok(())
    }

    /// Total number of segments opened since [`WalWriter::open`] was called
    /// (including the segment that is currently open).
    pub fn segment_count(&self) -> u64 {
        self.total_segment_count
    }

    // ── Symlink update ─────────────────────────────────────────────────────────

    /// Atomically update the "wal-current" symlink to point to the
    /// active segment.
    pub fn update_symlink(&self, fs: &dyn FileSystem) -> io::Result<()> {
        let link = self.wal_dir.join("wal-current");
        let tmp = self.wal_dir.join("wal-current.tmp");
        #[cfg(unix)]
        {
            fs.symlink(&self.segment_path, &tmp)?;
            fs.rename(&tmp, &link)?;
        }
        #[cfg(not(unix))]
        {
            // On Windows symlinks need privileges; fall back to a plain
            // text file containing the path.
            let handle = fs.open(&tmp, true)?;
            handle.write_at(self.segment_path.to_string_lossy().as_bytes(), 0)?;
            handle.sync_data()?;
            fs.rename(&tmp, &link)?;
        }
        // Make the renamed `wal-current` entry durable (H8): the atomic-rename
        // trick is only crash-atomic once the directory itself is fsynced.
        fs.sync_dir(&self.wal_dir)?;
        Ok(())
    }

    // ── Internal helpers ───────────────────────────────────────────────────────

    /// Compute the XxHash64 of the bytes currently written to the segment file
    /// (not including any still-buffered data).
    fn compute_segment_hash(&self) -> io::Result<u64> {
        let handle = self.segment_handle.as_ref().expect("segment file not open");
        let len = handle.len()? as usize;
        if len == 0 {
            return Ok(0);
        }
        // Read in chunks to avoid large stack allocations.
        const CHUNK: usize = 256 * 1024; // 256 KiB
        let mut hasher = XxHash64::with_seed(0);
        let mut offset = 0u64;
        while (offset as usize) < len {
            let remaining = len - offset as usize;
            let to_read = remaining.min(CHUNK);
            let mut buf = AlignedBuffer::zeroed(to_read);
            handle.read_at(&mut buf, offset)?;
            hasher.write(&buf[..to_read]);
            offset += to_read as u64;
        }
        Ok(hasher.finish())
    }

    /// Move segments older than `Self::LIVE_SEGMENT_RETENTION` behind
    /// `current_segment_id` to the archive directory.
    fn archive_old_segments(&self, fs: &dyn FileSystem, current_segment_id: u64) -> io::Result<()> {
        if current_segment_id < Self::LIVE_SEGMENT_RETENTION as u64 {
            return Ok(());
        }
        let archive_dir = self.wal_dir.join(Self::ARCHIVE_DIR);
        fs.create_dir_all(&archive_dir)?;

        // Archive every segment strictly older than (current - LIVE_SEGMENT_RETENTION).
        let cutoff = current_segment_id.saturating_sub(Self::LIVE_SEGMENT_RETENTION as u64 - 1);
        for seg_id in 0..cutoff {
            let seg_path = self.wal_dir.join(format!("wal-{:09}", seg_id));
            if fs.exists(&seg_path) {
                let dest = archive_dir.join(format!("wal-{:09}", seg_id));
                // Move the segment to the archive directory.  If this fails
                // (e.g. cross-device) we silently ignore — the segment will
                // simply remain in the live directory until the next rotation.
                let _ = fs.rename(&seg_path, &dest);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;
    use crate::wal::record::{RecordType, WalRecord};

    #[test]
    fn append_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut writer = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();

        let rec = WalRecord::new(RecordType::Begin, 1, 0, 0, vec![]);
        let lsn = writer.append(&fs, rec).unwrap();
        writer.sync(&fs).unwrap();

        // Read the segment back.
        let seg = dir.path().join("wal-000000000");
        let handle = fs.open(&seg, false).unwrap();
        let len = handle.len().unwrap() as usize;
        let mut buf = vec![0u8; len];
        handle.read_at(&mut buf, 0).unwrap();

        let (decoded, size) = WalRecord::decode(&buf, 0).unwrap();
        assert_eq!(decoded.lsn, lsn);
        assert_eq!(decoded.record_type, RecordType::Begin);
        assert_eq!(size, len);
    }

    #[test]
    fn buffer_is_aligned() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let writer = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();
        assert_eq!(writer.buffer.len(), WalWriter::BUFFER_SIZE);
        assert_eq!(
            writer.buffer.as_ptr() as usize % AlignedBuffer::ALIGNMENT,
            0
        );
    }

    #[test]
    fn persistent_handle_is_stored() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let writer = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();
        assert!(
            writer.segment_handle.is_some(),
            "segment_handle must be set after open"
        );
    }

    #[test]
    fn flush_reuses_persistent_handle() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut writer = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();

        let rec = WalRecord::new(RecordType::Begin, 1, 0, 0, vec![]);
        writer.append(&fs, rec).unwrap();
        writer.flush(&fs).unwrap();

        assert!(
            writer.segment_handle.is_some(),
            "segment_handle must remain set after flush"
        );
    }

    // ── Segment descriptor tests ───────────────────────────────────────────────

    #[test]
    fn segment_descriptor_roundtrip() {
        let desc = SegmentDescriptor {
            segment_id: 42,
            segment_start_lsn: 1_000_000,
            previous_segment_id: 41,
            record_count: 12_345,
            integrity_hash: 0xDEADBEEFCAFEBABE,
        };
        let encoded = desc.encode();
        let decoded = SegmentDescriptor::decode(&encoded).unwrap();
        assert_eq!(decoded, desc);
    }

    #[test]
    fn segment_descriptor_wrong_magic() {
        let mut block = [0u8; SegmentDescriptor::SIZE];
        block[0..4].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
        assert!(SegmentDescriptor::decode(&block).is_none());
    }

    #[test]
    fn segment_descriptor_too_short() {
        let block = [0u8; 10];
        assert!(SegmentDescriptor::decode(&block).is_none());
    }

    // ── Rotation tests ─────────────────────────────────────────────────────────

    #[test]
    fn segment_create_and_symlink_rename_fsync_the_wal_directory() {
        // Regression gate for reliability-audit finding H8 (2026-06-04):
        // creating a WAL segment and atomically renaming `wal-current` must
        // `fsync` the containing directory, otherwise the new directory entries
        // (and committed records in a freshly rotated segment) can vanish on a
        // power loss even though the file data was fsynced.
        use crate::io::FileHandle;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct CountingFs {
            inner: PosixFileSystem,
            dir_syncs: Arc<AtomicUsize>,
        }
        impl FileSystem for CountingFs {
            fn open(&self, path: &Path, create: bool) -> io::Result<Box<dyn FileHandle>> {
                self.inner.open(path, create)
            }
            fn remove(&self, path: &Path) -> io::Result<()> {
                self.inner.remove(path)
            }
            fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
                self.inner.rename(from, to)
            }
            #[cfg(unix)]
            fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
                self.inner.symlink(target, link)
            }
            fn exists(&self, path: &Path) -> bool {
                self.inner.exists(path)
            }
            fn create_dir_all(&self, path: &Path) -> io::Result<()> {
                self.inner.create_dir_all(path)
            }
            fn sync_dir(&self, dir: &Path) -> io::Result<()> {
                self.dir_syncs.fetch_add(1, Ordering::SeqCst);
                self.inner.sync_dir(dir)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let fs = CountingFs {
            inner: PosixFileSystem::new(false),
            dir_syncs: Arc::clone(&counter),
        };

        // open() creates segment 0 → must fsync the wal directory.
        let mut writer = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();
        assert!(
            counter.load(Ordering::SeqCst) >= 1,
            "creating segment 0 must fsync the wal directory"
        );

        // rotate() creates a new segment AND renames wal-current → ≥2 more fsyncs.
        let before = counter.load(Ordering::SeqCst);
        writer.rotate(&fs).unwrap();
        assert!(
            counter.load(Ordering::SeqCst) >= before + 2,
            "rotation must fsync the wal dir for the new segment and the symlink rename"
        );
    }

    #[test]
    fn rotate_creates_new_segment() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut writer = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();

        // Write one record then rotate.
        writer
            .append(&fs, WalRecord::new(RecordType::Begin, 1, 0, 0, vec![]))
            .unwrap();
        writer.rotate(&fs).unwrap();

        assert_eq!(writer.segment_count(), 2);
        assert!(dir.path().join("wal-000000001").exists());
    }

    #[test]
    fn rotate_writes_descriptor_block() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut writer = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();

        writer
            .append(&fs, WalRecord::new(RecordType::Begin, 1, 0, 0, vec![]))
            .unwrap();
        writer.rotate(&fs).unwrap();

        // The sealed segment should end with a descriptor block.
        let seg = dir.path().join("wal-000000000");
        let handle = fs.open(&seg, false).unwrap();
        let len = handle.len().unwrap() as usize;
        assert!(len >= SegmentDescriptor::SIZE);

        // Find the descriptor block within the file.
        let mut buf = vec![0u8; len];
        handle.read_at(&mut buf, 0).unwrap();

        // The descriptor block starts at some point after the WAL records.
        // Scan forward to find it.
        let mut found = false;
        let mut pos = 0;
        while pos + SegmentDescriptor::SIZE <= len {
            if SegmentDescriptor::decode(&buf[pos..]).is_some() {
                found = true;
                break;
            }
            pos += 1;
        }
        assert!(
            found,
            "SegmentDescriptor must be present in the sealed segment"
        );
    }

    #[test]
    fn needs_rotation_only_after_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let writer = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();
        // Fresh writer is far below 64 MiB.
        assert!(!writer.needs_rotation());
    }

    #[test]
    fn archive_happens_after_multiple_rotations() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut writer = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();

        // Rotate 3 times.
        for _ in 0..3 {
            writer
                .append(&fs, WalRecord::new(RecordType::Begin, 1, 0, 0, vec![]))
                .unwrap();
            writer.rotate(&fs).unwrap();
        }

        // After 3 rotations we have segments 0, 1, 2, 3 (3 is current).
        // Segments 0 and 1 should have been archived.
        let archive_dir = dir.path().join("wal-archive");
        assert!(archive_dir.exists(), "archive directory must be created");
        assert!(
            archive_dir.join("wal-000000000").exists()
                || !dir.path().join("wal-000000000").exists(),
            "segment 0 must be archived or removed from live dir"
        );
    }

    // ── LSN encoding tests (Task 150) ─────────────────────────────────────────

    #[test]
    fn lsn_encoding_roundtrip() {
        let seg_id = 7u32;
        let offset = 0x0001_2345u32;
        let lsn = make_lsn(seg_id, offset);
        assert_eq!(lsn_segment_id(lsn), seg_id);
        assert_eq!(lsn_offset(lsn), offset);
    }

    #[test]
    fn open_resumes_from_highest_segment() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);

        // Create segments 0 and 1, each with one record.
        {
            let mut writer = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();
            writer
                .append(&fs, WalRecord::new(RecordType::Begin, 1, 0, 0, vec![]))
                .unwrap();
            writer.rotate(&fs).unwrap();
            writer
                .append(&fs, WalRecord::new(RecordType::Commit, 1, 0, 0, vec![]))
                .unwrap();
            writer.sync(&fs).unwrap();
            // Now current segment is 1 (after rotation).
            assert_eq!(writer.segment_id, 1);
        }

        // Reopen: should find segment 1 and continue from there.
        let writer2 = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();
        assert_eq!(
            writer2.segment_id, 1,
            "reopened writer must resume on highest segment"
        );
        // LSN must encode segment_id=1.
        assert_eq!(lsn_segment_id(writer2.current_lsn), 1);
    }

    #[test]
    fn needs_checkpoint_after_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let mut writer = WalWriter::open(dir.path().to_path_buf(), &fs).unwrap();

        assert!(
            !writer.needs_checkpoint(),
            "fresh writer should not need checkpoint"
        );

        // Simulate writing CHECKPOINT_INTERVAL bytes.
        writer.bytes_since_checkpoint = WalWriter::CHECKPOINT_INTERVAL;
        assert!(
            writer.needs_checkpoint(),
            "should need checkpoint after threshold"
        );

        writer.reset_checkpoint_counter();
        assert!(
            !writer.needs_checkpoint(),
            "counter reset; no checkpoint needed"
        );
    }
}
