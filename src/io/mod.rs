use std::io;
use std::path::Path;

pub mod aligned_buffer;
pub mod posix;

pub use aligned_buffer::AlignedBuffer;

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
}

/// A handle to an open file.
pub trait FileHandle: Send + Sync {
    /// Read exactly `buf.len()` bytes from `offset` into `buf`.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;

    /// Write `buf` to `offset`.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;

    /// Flush all buffers and metadata to disk.
    fn sync_all(&self) -> io::Result<()>;

    /// Flush data buffers to disk (cheaper than `sync_all`).
    fn sync_data(&self) -> io::Result<()>;

    /// Current file size in bytes.
    fn len(&self) -> io::Result<u64>;

    /// Returns `true` if the file is empty (zero bytes).
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Set the file length (may extend or truncate).
    fn set_len(&self, len: u64) -> io::Result<()>;
}
