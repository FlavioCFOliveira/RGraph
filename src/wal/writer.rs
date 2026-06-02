use crate::io::FileSystem;
use crate::wal::record::WalRecord;
use std::io;
use std::path::PathBuf;

/// Append-only WAL writer that maintains a monotonic LSN.
#[derive(Debug)]
pub struct WalWriter {
    /// Directory that holds WAL segment files.
    pub wal_dir: PathBuf,
    /// Current LSN (byte offset from start of WAL).
    pub current_lsn: u64,
    /// Path of the current segment file.
    pub segment_path: PathBuf,
    /// In-memory buffer for records not yet synced.
    buffer: Vec<u8>,
    /// Number of bytes since last sync.
    unsynced: usize,
}

impl WalWriter {
    pub const SEGMENT_SIZE: u64 = 64 * 1024 * 1024; // 64 MB
    pub const BUFFER_SIZE: usize = 256 * 1024;       // 256 KB

    /// Open (or create) the WAL in `wal_dir`.
    pub fn open(wal_dir: PathBuf, fs: &dyn FileSystem) -> io::Result<Self> {
        fs.create_dir_all(&wal_dir)?;
        let segment_path = wal_dir.join("wal-000000000");
        let current_lsn = if fs.exists(&segment_path) {
            let handle = fs.open(&segment_path, false)?;
            let len = handle.len()?;
            len
        } else {
            let handle = fs.open(&segment_path, true)?;
            handle.sync_data()?;
            1 // LSN 0 is reserved for "never written".
        };
        Ok(Self {
            wal_dir,
            current_lsn,
            segment_path,
            buffer: Vec::with_capacity(Self::BUFFER_SIZE),
            unsynced: 0,
        })
    }

    /// Append a record and return its LSN.
    pub fn append(&mut self, fs: &dyn FileSystem, mut record: WalRecord) -> io::Result<u64> {
        record.set_lsn(self.current_lsn);
        let bytes = record.encode();
        self.buffer.extend_from_slice(&bytes);
        self.unsynced += bytes.len();
        self.current_lsn += bytes.len() as u64;

        if self.buffer.len() >= Self::BUFFER_SIZE {
            self.flush(fs)?;
        }
        Ok(record.lsn)
    }

    /// Flush buffered data to disk and sync.
    pub fn flush(&mut self, fs: &dyn FileSystem) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let handle = fs.open(&self.segment_path, true)?;
        let offset = handle.len()?;
        handle.write_at(&self.buffer, offset)?;
        handle.sync_data()?;
        self.buffer.clear();
        self.unsynced = 0;
        Ok(())
    }

    /// Sync any pending data to durable storage.
    pub fn sync(&mut self, fs: &dyn FileSystem) -> io::Result<()> {
        self.flush(fs)
    }

    /// Atomically update the "wal-current" symlink to point to the
    /// active segment.  (Used later when segment rotation is added.)
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
}
