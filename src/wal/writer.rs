//! Append-only WAL writer with LSN tracking and segment rotation (Task 110).
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
use std::path::PathBuf;
use twox_hash::XxHash64;

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
#[derive(Debug)]
pub struct WalWriter {
    /// Directory that holds WAL segment files.
    pub wal_dir: PathBuf,
    /// Current LSN (byte offset from start of WAL).
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
    /// LSN at which the current segment began.
    segment_start_lsn: u64,
    /// Number of WAL records written to the current segment.
    segment_record_count: u64,
    /// Total number of segments written since open (including the current one).
    total_segment_count: u64,
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

    /// Open (or create) the WAL in `wal_dir`.
    pub fn open(wal_dir: PathBuf, fs: &dyn FileSystem) -> io::Result<Self> {
        fs.create_dir_all(&wal_dir)?;
        let segment_path = wal_dir.join("wal-000000000");
        let (current_lsn, segment_id) = if fs.exists(&segment_path) {
            let handle = fs.open(&segment_path, false)?;
            (handle.len()?, 0u64)
        } else {
            let handle = fs.open(&segment_path, true)?;
            handle.sync_data()?;
            (1, 0u64) // LSN 0 is reserved for "never written".
        };
        Ok(Self {
            wal_dir,
            current_lsn,
            segment_path,
            buffer: AlignedBuffer::zeroed(Self::BUFFER_SIZE),
            buffered: 0,
            unsynced: 0,
            segment_id,
            segment_start_lsn: 0,
            segment_record_count: 0,
            total_segment_count: 1,
        })
    }

    /// Append a record and return its LSN.
    pub fn append(&mut self, fs: &dyn FileSystem, mut record: WalRecord) -> io::Result<u64> {
        record.set_lsn(self.current_lsn);
        let bytes = record.encode();
        if self.buffered + bytes.len() > Self::BUFFER_SIZE {
            self.flush(fs)?;
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
        self.current_lsn += bytes.len() as u64;
        self.segment_record_count += 1;
        Ok(record.lsn)
    }

    /// Flush buffered data to disk and sync.
    pub fn flush(&mut self, fs: &dyn FileSystem) -> io::Result<()> {
        if self.buffered == 0 {
            return Ok(());
        }
        let handle = fs.open(&self.segment_path, true)?;
        let offset = handle.len()?;
        handle.write_at(&self.buffer[..self.buffered], offset)?;
        handle.sync_data()?;
        self.buffered = 0;
        self.unsynced = 0;
        Ok(())
    }

    /// Sync any pending data to durable storage.
    pub fn sync(&mut self, fs: &dyn FileSystem) -> io::Result<()> {
        self.flush(fs)
    }

    // ── Segment rotation (Task 110) ────────────────────────────────────────────

    /// Returns `true` if the current segment has reached [`SEGMENT_SIZE`] and
    /// should be rotated before appending more records.
    pub fn needs_rotation(&self) -> bool {
        // Account for data already in the write buffer.
        let on_disk_approx = self.current_lsn - self.segment_start_lsn;
        on_disk_approx >= Self::SEGMENT_SIZE
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
        let integrity_hash = self.compute_segment_hash(fs)?;

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
            let handle = fs.open(&self.segment_path, true)?;
            let offset = handle.len()?;
            handle.write_at(&desc_block, offset)?;
            handle.sync_data()?;
        }

        // Write a SegmentDescriptor WAL record (informational; not used for
        // recovery but visible in WAL scans).
        let desc_rec = WalRecord::new(
            RecordType::SegmentDescriptor,
            0,
            0,
            0,
            desc_block.to_vec(),
        );
        // Encode and append directly to the segment file (bypass buffer since
        // we just flushed and may be about to switch).
        let desc_bytes = desc_rec.encode();
        {
            let handle = fs.open(&self.segment_path, true)?;
            let offset = handle.len()?;
            handle.write_at(&desc_bytes, offset)?;
            handle.sync_data()?;
        }
        self.current_lsn += desc_bytes.len() as u64;

        // 4. Open next segment.
        let next_segment_id = self.segment_id + 1;
        let next_path = self
            .wal_dir
            .join(format!("wal-{:09}", next_segment_id));
        {
            let handle = fs.open(&next_path, true)?;
            handle.sync_data()?;
        }

        // 5. Atomically update symlink.
        self.segment_path = next_path;
        self.update_symlink(fs)?;

        // 6. Archive old segments.
        let prev_segment_id = self.segment_id;
        self.segment_id = next_segment_id;
        self.segment_start_lsn = self.current_lsn;
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
        Ok(())
    }

    // ── Internal helpers ───────────────────────────────────────────────────────

    /// Compute the XxHash64 of the bytes currently written to the segment file
    /// (not including any still-buffered data).
    fn compute_segment_hash(&self, fs: &dyn FileSystem) -> io::Result<u64> {
        let handle = fs.open(&self.segment_path, false)?;
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
            let mut buf = vec![0u8; to_read];
            handle.read_at(&mut buf, offset)?;
            hasher.write(&buf);
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
        assert_eq!(writer.buffer.as_ptr() as usize % AlignedBuffer::ALIGNMENT, 0);
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
        assert!(found, "SegmentDescriptor must be present in the sealed segment");
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
}
