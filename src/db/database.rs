use crate::io::{AlignedBuffer, FileSystem};
use crate::storage::meta::decode_superblock;
use crate::storage::page::PAGE_SIZE;
use crate::storage::manager::PageManager;
use crate::wal::recovery::{recover, simple_page_replay};
use crate::wal::writer::WalWriter;
use std::io;
use std::path::{Path, PathBuf};

/// Top-level database handle.
#[derive(Debug)]
pub struct Database {
    pub path: PathBuf,
    pub page_manager: PageManager,
    pub wal_writer: WalWriter,
}

impl Database {
    pub const LOCK_FILE: &str = ".rgraph.lock";

    /// Create a new empty database at `path`.
    pub fn init(path: &Path, fs: &dyn FileSystem) -> io::Result<Self> {
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "database path already exists",
            ));
        }
        fs.create_dir_all(path)?;

        // Advisory lock to prevent double-open.
        let lock = path.join(Self::LOCK_FILE);
        {
            let handle = fs.open(&lock, true)?;
            handle.sync_data()?;
        }

        let data_path = path.join(PageManager::DATA_FILE);
        let mut pm = PageManager::init(data_path.clone(), PAGE_SIZE as u32, fs)?;

        // Write superblock (both copies).
        pm.sync_superblock(fs)?;

        // Write bitmap page.
        pm.sync_bitmap(fs)?;

        // Initialise WAL with a buffered filesystem (WAL does not use O_DIRECT).
        let wal_dir = path.join("wal");
        let wal_fs = crate::io::posix::PosixFileSystem::new(false);
        let wal_writer = WalWriter::open(wal_dir, &wal_fs)?;

        Ok(Self {
            path: path.to_path_buf(),
            page_manager: pm,
            wal_writer,
        })
    }

    /// Open an existing database, recovering WAL if necessary.
    pub fn open(path: &Path, fs: &dyn FileSystem) -> io::Result<Self> {
        if !path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "database path does not exist",
            ));
        }

        // Check advisory lock.
        let lock = path.join(Self::LOCK_FILE);
        if !fs.exists(&lock) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "database lock file missing; partial init?",
            ));
        }

        let data_path = path.join(PageManager::DATA_FILE);
        if !fs.exists(&data_path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "database data file missing",
            ));
        }

        // Read both superblock copies.
        let handle = fs.open(&data_path, false)?;
        let mut primary = AlignedBuffer::zeroed(PAGE_SIZE);
        let mut mirror = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut primary, 0)?;
        handle.read_at(&mut mirror, PAGE_SIZE as u64)?;

        let sb_primary = decode_superblock(&primary);
        let sb_mirror = decode_superblock(&mirror);

        let sb = match (sb_primary, sb_mirror) {
            (Some(p), Some(m)) => {
                if m.generation > p.generation {
                    m
                } else {
                    p
                }
            }
            (Some(p), None) => p,
            (None, Some(m)) => m,
            (None, None) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "both superblock copies invalid",
                ));
            }
        };

        // Read bitmap page.
        let mut bitmap_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut bitmap_buf, (2 * PAGE_SIZE) as u64)?;

        let mut pm = PageManager::open(data_path.clone(), sb, bitmap_buf, fs)?;

        // Recover WAL using a buffered filesystem (WAL does not use O_DIRECT).
        let wal_dir = path.join("wal");
        let wal_path = wal_dir.join("wal-000000000");
        let wal_fs = crate::io::posix::PosixFileSystem::new(false);
        let start_lsn = pm.superblock.last_checkpoint_lsn;
        if let Some(last_lsn) = recover(
            &wal_fs,
            &wal_path,
            start_lsn,
            |pid, img, lsn| simple_page_replay(fs, &data_path, pid, img, lsn),
        )? {
            pm.superblock.current_wal_lsn = last_lsn;
            pm.sync_superblock(fs)?;
        }

        let wal_writer = WalWriter::open(wal_dir, &wal_fs)?;

        Ok(Self {
            path: path.to_path_buf(),
            page_manager: pm,
            wal_writer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;

    #[test]
    fn init_and_open_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        {
            let db = Database::init(&db_path, &fs).unwrap();
            assert_eq!(db.page_manager.superblock.total_page_count, 3);
        }

        {
            let db = Database::open(&db_path, &fs).unwrap();
            assert_eq!(db.page_manager.superblock.total_page_count, 3);
        }
    }

    #[test]
    fn open_rejects_missing_lock() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");
        fs.create_dir_all(&db_path).unwrap();
        let data = db_path.join(PageManager::DATA_FILE);
        fs.open(&data, true).unwrap().sync_data().unwrap();

        let err = Database::open(&db_path, &fs).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn corrupt_primary_superblock_recover_from_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        {
            let db = Database::init(&db_path, &fs).unwrap();
            assert_eq!(db.page_manager.superblock.total_page_count, 3);
            drop(db);
        }

        // Corrupt the primary superblock (offset 0).
        let data_path = db_path.join(PageManager::DATA_FILE);
        let handle = fs.open(&data_path, true).unwrap();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, 0).unwrap();
        buf[20] ^= 0xFF;
        handle.write_at(&buf, 0).unwrap();
        handle.sync_all().unwrap();
        drop(handle);

        // Open should recover from the mirror copy.
        let db = Database::open(&db_path, &fs).unwrap();
        assert_eq!(db.page_manager.superblock.total_page_count, 3);
    }

    #[test]
    fn corrupt_mirror_superblock_recover_from_primary() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        {
            let db = Database::init(&db_path, &fs).unwrap();
            drop(db);
        }

        // Corrupt the mirror superblock (offset PAGE_SIZE).
        let data_path = db_path.join(PageManager::DATA_FILE);
        let handle = fs.open(&data_path, true).unwrap();
        let mut buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut buf, PAGE_SIZE as u64).unwrap();
        buf[20] ^= 0xFF;
        handle.write_at(&buf, PAGE_SIZE as u64).unwrap();
        handle.sync_all().unwrap();
        drop(handle);

        // Open should recover from the primary copy.
        let db = Database::open(&db_path, &fs).unwrap();
        assert_eq!(db.page_manager.superblock.total_page_count, 3);
    }

    #[test]
    fn crash_mid_superblock_write_recovers_last_good_generation() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PosixFileSystem::new(false);
        let db_path = dir.path().join("db");

        {
            let db = Database::init(&db_path, &fs).unwrap();
            drop(db);
        }

        let data_path = db_path.join(PageManager::DATA_FILE);
        let handle = fs.open(&data_path, true).unwrap();

        // Read the current mirror superblock to get the last good generation.
        let mut mirror_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut mirror_buf, PAGE_SIZE as u64).unwrap();
        let sb_mirror = decode_superblock(&mirror_buf).unwrap();
        let old_generation = sb_mirror.generation;

        // Simulate a torn write: write a newer generation to primary, then corrupt it.
        let mut primary_buf = AlignedBuffer::zeroed(PAGE_SIZE);
        handle.read_at(&mut primary_buf, 0).unwrap();
        let mut sb = decode_superblock(&primary_buf).unwrap();
        sb.generation += 1;
        sb.update_checksum();
        let encoded = crate::storage::meta::encode_superblock(&sb);
        primary_buf[..encoded.len()].copy_from_slice(&encoded);
        primary_buf[30] ^= 0xFF; // corrupt the primary
        handle.write_at(&primary_buf, 0).unwrap();

        // Mirror remains with the old generation.
        handle.write_at(&mirror_buf, PAGE_SIZE as u64).unwrap();
        handle.sync_all().unwrap();
        drop(handle);

        // Open should recover from the mirror with the last good generation.
        let db = Database::open(&db_path, &fs).unwrap();
        assert_eq!(db.page_manager.superblock.generation, old_generation);
    }
}
