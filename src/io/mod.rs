use std::io;
use std::path::Path;

pub mod aligned_buffer;
pub mod posix;

#[cfg(target_os = "linux")]
pub mod uring;

pub mod fault;

pub use aligned_buffer::AlignedBuffer;
pub use fault::{DeterministicIoUring, FaultConfig, FaultInjectFileSystem, FaultKind, FaultRule, OpMask};

/// A portable abstraction over file-system operations.
///
/// Implementations are expected to work with page-aligned buffers
/// so that they can be passed directly to the kernel without
/// extra copies (required for `O_DIRECT` and `io_uring`).
pub trait FileSystem: Send + Sync {
    /// Open (or create) a file at `path` for read/write.
    fn open(&self, path: &Path, create: bool) -> io::Result<Box<dyn FileHandle>>;

    /// Remove a file at `path`.
    fn remove(&self, path: &Path) -> io::Result<()>;

    /// Rename `from` to `to` atomically.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Create a symlink at `link` pointing to `target`.
    #[cfg(unix)]
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()>;

    /// Check whether `path` exists.
    fn exists(&self, path: &Path) -> bool;

    /// Create a directory (and any parent directories).
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// `fsync` the directory `dir` so that newly created or renamed entries
    /// inside it become durable.
    ///
    /// POSIX guarantees the *contents* of a file are durable after
    /// `fsync(file)`, but the *directory entry* (created by `creat`/`rename`/
    /// `link`) is only guaranteed durable after an explicit `fsync` of the
    /// containing directory.  Without this, a freshly created WAL segment — or
    /// the atomically-renamed `wal-current` symlink — can vanish on power loss
    /// even though its data was fsynced, losing committed records.  See
    /// reliability-audit finding H8 (2026-06-04).
    ///
    /// The default implementation opens the directory read-only and `fsync`s
    /// it, which is the correct behaviour on Unix.  On non-Unix targets it is a
    /// no-op (directory fds cannot be fsynced portably).
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            let handle = std::fs::File::open(dir)?;
            handle.sync_all()
        }
        #[cfg(not(unix))]
        {
            let _ = dir;
            Ok(())
        }
    }
}

/// A handle to an open file.
pub trait FileHandle: Send + Sync {
    /// Read exactly `buf.len()` bytes from `offset` into `buf`.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;

    /// Vectored read: fill each buffer slice sequentially starting at `offset`.
    ///
    /// The default implementation calls `read_at` for each slice in order.
    fn readv_at(&self, bufs: &mut [&mut [u8]], offset: u64) -> io::Result<()> {
        let mut off = offset;
        for buf in bufs {
            self.read_at(buf, off)?;
            off += buf.len() as u64;
        }
        Ok(())
    }

    /// Write `buf` to `offset`.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;

    /// Vectored write: write each buffer slice sequentially starting at `offset`.
    ///
    /// The default implementation calls `write_at` for each slice in order.
    fn writev_at(&self, bufs: &[&[u8]], offset: u64) -> io::Result<()> {
        let mut off = offset;
        for buf in bufs {
            self.write_at(buf, off)?;
            off += buf.len() as u64;
        }
        Ok(())
    }

    /// Flush all buffers and metadata to disk.
    fn sync_all(&self) -> io::Result<()>;

    /// Flush data buffers to disk (cheaper than `sync_all`).
    fn sync_data(&self) -> io::Result<()>;

    /// Advise the kernel that this file will be accessed randomly,
    /// disabling readahead.  Default is a no-op.
    fn advise_random(&self) -> io::Result<()> {
        Ok(())
    }

    /// Current file size in bytes.
    fn len(&self) -> io::Result<u64>;

    /// Returns `true` if the file is empty (zero bytes).
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Set the file length (may extend or truncate).
    fn set_len(&self, len: u64) -> io::Result<()>;
}
