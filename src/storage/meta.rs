use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::page::{PageId, PAGE_SIZE};
use std::io;
use std::mem::size_of;
use std::path::Path;

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
    /// Persisted graph data model: `0` = unspecified (legacy databases created
    /// before this field existed), `1` = LPG, `2` = RDF.  Validated on open so a
    /// database created under one model can never be silently reopened under the
    /// other (finding M2).  Covered by `checksum`.
    pub graph_mode: u8,
    /// Reserved padding to keep the struct at 128 bytes.
    pub _reserved: [u8; 51],
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
            graph_mode: 0,
            _reserved: [0; 51],
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
///
/// Returns an [`AlignedBuffer`] padded to [`PAGE_SIZE`] for direct I/O.
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

/// Why a superblock buffer failed validation.
///
/// Distinguishes genuine corruption (bad magic / checksum) from compatibility
/// problems (unsupported version, wrong page size, opposite-endian file), so the
/// reported error is actionable rather than a misleading "checksum failure"
/// (finding L14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuperblockError {
    /// Buffer too short to contain a superblock.
    TooShort,
    /// Magic signature absent — not an RGraph database or severe corruption.
    BadMagic,
    /// Magic matches the byte-swapped signature: the file was written on a
    /// machine of the opposite endianness.  The on-disk format is native-endian
    /// (the magic doubles as the endianness marker), so such a file is not
    /// portable and must be rebuilt on this architecture.
    EndiannessMismatch,
    /// CRC32C mismatch — the superblock is corrupt.
    ChecksumMismatch,
    /// Format version this build does not support.
    UnsupportedVersion { found: u32, supported: u32 },
    /// Page size differs from this build's `PAGE_SIZE`.
    WrongPageSize { found: u32, expected: u32 },
}

impl std::fmt::Display for SuperblockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SuperblockError::TooShort => write!(f, "superblock buffer too short"),
            SuperblockError::BadMagic => {
                write!(f, "not an RGraph superblock (bad magic) or severe corruption")
            }
            SuperblockError::EndiannessMismatch => write!(
                f,
                "superblock was written on a machine of the opposite endianness \
                 (native-endian on-disk format); rebuild the database on this architecture"
            ),
            SuperblockError::ChecksumMismatch => write!(f, "superblock checksum mismatch (corrupt)"),
            SuperblockError::UnsupportedVersion { found, supported } => write!(
                f,
                "unsupported superblock format version {found} (this build supports {supported})"
            ),
            SuperblockError::WrongPageSize { found, expected } => write!(
                f,
                "superblock page size {found} differs from this build's page size {expected}"
            ),
        }
    }
}

impl SuperblockError {
    /// The most appropriate [`io::ErrorKind`]: compatibility problems map to
    /// `Unsupported`, genuine corruption to `InvalidData`.
    fn error_kind(&self) -> io::ErrorKind {
        match self {
            SuperblockError::UnsupportedVersion { .. }
            | SuperblockError::WrongPageSize { .. }
            | SuperblockError::EndiannessMismatch => io::ErrorKind::Unsupported,
            SuperblockError::TooShort
            | SuperblockError::BadMagic
            | SuperblockError::ChecksumMismatch => io::ErrorKind::InvalidData,
        }
    }

    /// Specificity rank — a higher value is a more informative diagnosis when
    /// both copies fail (a clear compatibility reason beats generic corruption).
    fn specificity(&self) -> u8 {
        match self {
            SuperblockError::EndiannessMismatch => 5,
            SuperblockError::UnsupportedVersion { .. } => 4,
            SuperblockError::WrongPageSize { .. } => 3,
            SuperblockError::ChecksumMismatch => 2,
            SuperblockError::BadMagic => 1,
            SuperblockError::TooShort => 0,
        }
    }
}

/// Validate a superblock buffer, returning the decoded superblock or the
/// specific reason it is unacceptable.
///
/// Endianness and magic are checked before the (endianness-sensitive) checksum
/// so an opposite-endian file is reported as such rather than as a checksum
/// failure.
pub fn validate_superblock(buf: &[u8]) -> Result<Superblock, SuperblockError> {
    if buf.len() < size_of::<Superblock>() {
        return Err(SuperblockError::TooShort);
    }
    let sb = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const Superblock) };
    if sb.magic != META_MAGIC {
        if sb.magic == META_MAGIC.swap_bytes() {
            return Err(SuperblockError::EndiannessMismatch);
        }
        return Err(SuperblockError::BadMagic);
    }
    if !sb.verify_checksum() {
        return Err(SuperblockError::ChecksumMismatch);
    }
    if sb.version != FORMAT_VERSION {
        return Err(SuperblockError::UnsupportedVersion {
            found: sb.version,
            supported: FORMAT_VERSION,
        });
    }
    if sb.page_size != PAGE_SIZE as u32 {
        return Err(SuperblockError::WrongPageSize {
            found: sb.page_size,
            expected: PAGE_SIZE as u32,
        });
    }
    Ok(sb)
}

/// Decode a superblock from a raw page buffer, returning `None` for any
/// validation failure.  Use [`validate_superblock`] when the specific reason is
/// needed.
pub fn decode_superblock(buf: &[u8]) -> Option<Superblock> {
    validate_superblock(buf).ok()
}

/// Open the database file at `path` and recover the best-available superblock.
///
/// The format stores two copies of the superblock:
/// - Primary copy at byte offset 0 (page 0).
/// - Mirror copy at byte offset `PAGE_SIZE` (page 1).
///
/// This function reads both copies, validates their checksums, and returns
/// the valid copy with the higher `generation` field.  If both copies are
/// invalid, it returns an [`io::Error`] of kind [`io::ErrorKind::InvalidData`].
///
/// # Errors
///
/// Returns an error if the file cannot be opened, if the file is too short
/// (must be at least `2 * PAGE_SIZE` bytes), or if both superblock copies
/// fail checksum validation.
pub fn load_superblock(fs: &dyn FileSystem, path: &Path) -> io::Result<Superblock> {
    let handle = fs.open(path, false)?;
    let file_len = handle.len()?;
    let min_len = 2 * PAGE_SIZE as u64;
    if file_len < min_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "database file too short: expected at least {} bytes, found {}",
                min_len, file_len
            ),
        ));
    }

    let mut primary_buf = AlignedBuffer::zeroed(PAGE_SIZE);
    let mut mirror_buf = AlignedBuffer::zeroed(PAGE_SIZE);
    handle.read_at(&mut primary_buf, 0)?;
    handle.read_at(&mut mirror_buf, PAGE_SIZE as u64)?;

    let primary = validate_superblock(&primary_buf);
    let mirror = validate_superblock(&mirror_buf);

    match (primary, mirror) {
        (Ok(p), Ok(m)) => {
            // Both are valid: prefer the higher generation.
            if m.generation > p.generation {
                Ok(m)
            } else {
                Ok(p)
            }
        }
        (Ok(p), Err(_)) => Ok(p),
        (Err(_), Ok(m)) => Ok(m),
        (Err(ep), Err(em)) => {
            // Both invalid: report the more informative reason (a clear
            // version/page-size/endianness problem beats generic corruption), so
            // a future-version or opposite-endian file is not misreported as a
            // checksum failure (finding L14).
            let reason = if em.specificity() > ep.specificity() {
                em
            } else {
                ep
            };
            Err(io::Error::new(reason.error_kind(), reason.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;

    #[test]
    fn superblock_size_is_128() {
        assert_eq!(size_of::<Superblock>(), 128);
    }

    /// Regression gate for finding L14 (Task 218, 2026-06-05): a future-version
    /// superblock with a valid checksum must be reported as an unsupported-version
    /// error, not a misleading checksum failure.
    #[test]
    fn load_superblock_reports_unsupported_version_not_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let fs = PosixFileSystem::new(false);

        // A v2 superblock with a VALID checksum, written to both copies.
        let mut sb = Superblock::new(PAGE_SIZE as u32);
        sb.version = FORMAT_VERSION + 1;
        sb.update_checksum();
        let enc = encode_superblock(&sb);
        let handle = fs.open(&path, true).unwrap();
        handle.set_len(2 * PAGE_SIZE as u64).unwrap();
        handle.write_at(&enc, 0).unwrap();
        handle.write_at(&enc, PAGE_SIZE as u64).unwrap();
        handle.sync_all().unwrap();
        drop(handle);

        let err = load_superblock(&fs, &path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("version"), "must report an unsupported version, got: {msg}");
        assert!(
            !msg.contains("checksum"),
            "must NOT report a checksum failure, got: {msg}"
        );
    }

    /// Finding L14: an opposite-endian file (byte-swapped magic) is detected as
    /// an endianness mismatch rather than corruption.
    #[test]
    fn validate_superblock_detects_endianness_mismatch() {
        let mut sb = Superblock::new(PAGE_SIZE as u32);
        sb.update_checksum();
        let mut enc = encode_superblock(&sb);
        enc[..8].copy_from_slice(&META_MAGIC.swap_bytes().to_ne_bytes());
        assert!(matches!(
            validate_superblock(&enc),
            Err(SuperblockError::EndiannessMismatch)
        ));
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

    /// Helper: write two valid superblock copies to a temp file.
    fn write_two_copies(
        fs: &PosixFileSystem,
        path: &std::path::Path,
        primary: &Superblock,
        mirror: &Superblock,
    ) {
        use std::io::Write;
        {
            let mut f = std::fs::File::create(path).unwrap();
            f.set_len(2 * PAGE_SIZE as u64).unwrap();
            f.flush().unwrap();
        }
        let handle = fs.open(path, false).unwrap();
        let enc_primary = encode_superblock(primary);
        let enc_mirror = encode_superblock(mirror);
        handle.write_at(&enc_primary, 0).unwrap();
        handle.write_at(&enc_mirror, PAGE_SIZE as u64).unwrap();
        handle.sync_data().unwrap();
    }

    #[test]
    fn load_superblock_primary_corrupted_returns_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let fs = PosixFileSystem::new(false);

        let mut sb = Superblock::new(PAGE_SIZE as u32);
        sb.generation = 1;
        sb.update_checksum();

        write_two_copies(&fs, &path, &sb, &sb);

        // Corrupt the primary copy.
        let handle = fs.open(&path, false).unwrap();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, 0).unwrap();
        buf[20] ^= 0xFF;
        handle.write_at(&buf, 0).unwrap();
        handle.sync_data().unwrap();
        drop(handle);

        let recovered = load_superblock(&fs, &path).unwrap();
        assert_eq!(recovered.generation, 1);
    }

    #[test]
    fn load_superblock_mirror_corrupted_returns_primary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let fs = PosixFileSystem::new(false);

        let mut sb = Superblock::new(PAGE_SIZE as u32);
        sb.generation = 2;
        sb.update_checksum();

        write_two_copies(&fs, &path, &sb, &sb);

        // Corrupt the mirror copy.
        let handle = fs.open(&path, false).unwrap();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, PAGE_SIZE as u64).unwrap();
        buf[20] ^= 0xFF;
        handle.write_at(&buf, PAGE_SIZE as u64).unwrap();
        handle.sync_data().unwrap();
        drop(handle);

        let recovered = load_superblock(&fs, &path).unwrap();
        assert_eq!(recovered.generation, 2);
    }

    #[test]
    fn load_superblock_prefers_higher_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let fs = PosixFileSystem::new(false);

        let mut sb_old = Superblock::new(PAGE_SIZE as u32);
        sb_old.generation = 3;
        sb_old.update_checksum();

        let mut sb_new = Superblock::new(PAGE_SIZE as u32);
        sb_new.generation = 7;
        sb_new.total_page_count = 42;
        sb_new.update_checksum();

        // Primary has generation 3, mirror has generation 7.
        write_two_copies(&fs, &path, &sb_old, &sb_new);

        let recovered = load_superblock(&fs, &path).unwrap();
        assert_eq!(recovered.generation, 7);
        assert_eq!(recovered.total_page_count, 42);
    }

    #[test]
    fn load_superblock_both_corrupted_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let fs = PosixFileSystem::new(false);

        let mut sb = Superblock::new(PAGE_SIZE as u32);
        sb.generation = 1;
        sb.update_checksum();

        write_two_copies(&fs, &path, &sb, &sb);

        // Corrupt both copies.
        let handle = fs.open(&path, false).unwrap();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, 0).unwrap();
        buf[20] ^= 0xFF;
        handle.write_at(&buf, 0).unwrap();
        handle.read_at(&mut buf, PAGE_SIZE as u64).unwrap();
        buf[20] ^= 0xFF;
        handle.write_at(&buf, PAGE_SIZE as u64).unwrap();
        handle.sync_data().unwrap();
        drop(handle);

        let result = load_superblock(&fs, &path);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
