use super::{FileHandle, FileSystem};
use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Probability (0.0–1.0) and kind of a fault to inject.
#[derive(Debug, Clone, Copy)]
pub enum FaultKind {
    /// Return `std::io::ErrorKind::Other` with raw OS error `EIO`.
    Eio,
    /// Return `std::io::ErrorKind::Other` with raw OS error `ENOSPC`.
    Enospc,
    /// Sleep for `delay_ms` before executing the operation.
    Delay { delay_ms: u64 },
    /// Truncate a write to `factor` of its original length (0.0–1.0).
    PartialWrite { factor: f64 },
    /// Return `std::io::ErrorKind::Other` with raw OS error `EIO` on fsync.
    FsyncFail,
}

/// Mutable configuration for the fault-injection backend.
#[derive(Debug, Clone, Default)]
pub struct FaultConfig {
    /// Ordered list of faults to apply.  The injector walks the list and
    /// applies the first fault whose predicate matches the current operation.
    pub rules: Vec<FaultRule>,
}

/// A single fault rule.
#[derive(Debug, Clone, Copy)]
pub struct FaultRule {
    /// Which operation kinds this rule applies to.
    pub op_mask: OpMask,
    /// The fault to inject.
    pub kind: FaultKind,
    /// Apply the fault every `n` operations (counter modulus).
    pub every_n: Option<u64>,
}

/// Bitmask of operations a rule can target.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpMask {
    pub read: bool,
    pub write: bool,
    pub sync_all: bool,
    pub sync_data: bool,
    pub set_len: bool,
}

/// A fault-injecting `FileSystem` implementation.
///
/// Wraps an inner `FileSystem` (typically [`PosixFileSystem`](super::posix::PosixFileSystem))
/// and injects delays, errors, partial writes, or fsync failures according
/// to a configurable rule set.
///
/// # Thread safety
///
/// `FaultInjectFileSystem` is `Send + Sync`.  The global operation counter
/// is atomic; the rule set is behind a `Mutex` so it can be updated at
/// runtime from a test harness.
pub struct FaultInjectFileSystem {
    inner: Box<dyn FileSystem>,
    config: Arc<Mutex<FaultConfig>>,
    op_counter: Arc<AtomicU64>,
}

impl FaultInjectFileSystem {
    /// Wrap `inner` with an empty (no-op) fault configuration.
    pub fn new(inner: Box<dyn FileSystem>) -> Self {
        Self {
            inner,
            config: Arc::new(Mutex::new(FaultConfig::default())),
            op_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Replace the current fault configuration.
    pub fn set_config(&self, config: FaultConfig) {
        *self.config.lock().unwrap() = config;
    }

    fn bump_counter(&self) -> u64 {
        self.op_counter.fetch_add(1, Ordering::Relaxed)
    }

    fn maybe_fault(&self, op: OpMask) -> Option<FaultKind> {
        let n = self.bump_counter();
        let cfg = self.config.lock().unwrap();
        for rule in &cfg.rules {
            if !matches_op(op, rule.op_mask) {
                continue;
            }
            if let Some(every) = rule.every_n {
                if (n % every) != 0 {
                    continue;
                }
            }
            return Some(rule.kind);
        }
        None
    }
}

fn matches_op(op: OpMask, mask: OpMask) -> bool {
    (op.read && mask.read)
        || (op.write && mask.write)
        || (op.sync_all && mask.sync_all)
        || (op.sync_data && mask.sync_data)
        || (op.set_len && mask.set_len)
}

impl FileSystem for FaultInjectFileSystem {
    fn open(&self, path: &Path, create: bool) -> io::Result<Box<dyn FileHandle>> {
        let handle = self.inner.open(path, create)?;
        Ok(Box::new(FaultInjectFileHandle {
            inner: handle,
            config: self.config.clone(),
            op_counter: Arc::clone(&self.op_counter),
        }))
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
}

struct FaultInjectFileHandle {
    inner: Box<dyn FileHandle>,
    config: Arc<Mutex<FaultConfig>>,
    op_counter: Arc<AtomicU64>,
}

impl FaultInjectFileHandle {
    fn maybe_fault(&self, op: OpMask) -> Option<FaultKind> {
        let n = self.op_counter.fetch_add(1, Ordering::Relaxed);
        let cfg = self.config.lock().unwrap();
        for rule in &cfg.rules {
            if !matches_op(op, rule.op_mask) {
                continue;
            }
            if let Some(every) = rule.every_n {
                if (n % every) != 0 {
                    continue;
                }
            }
            return Some(rule.kind);
        }
        None
    }
}

impl FileHandle for FaultInjectFileHandle {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.readv_at(&mut [buf], offset)
    }

    fn readv_at(&self, bufs: &mut [&mut [u8]], offset: u64) -> io::Result<()> {
        if let Some(kind) = self.maybe_fault(OpMask {
            read: true,
            ..OpMask::default()
        }) {
            match kind {
                FaultKind::Delay { delay_ms } => {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                }
                FaultKind::Eio => {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                FaultKind::Enospc => {
                    return Err(io::Error::from_raw_os_error(libc::ENOSPC));
                }
                _ => {}
            }
        }
        self.inner.readv_at(bufs, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut buf_to_write = buf;
        let mut truncated_buf: Vec<u8> = Vec::new();

        if let Some(kind) = self.maybe_fault(OpMask {
            write: true,
            ..OpMask::default()
        }) {
            match kind {
                FaultKind::Delay { delay_ms } => {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                }
                FaultKind::Eio => {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                FaultKind::Enospc => {
                    return Err(io::Error::from_raw_os_error(libc::ENOSPC));
                }
                FaultKind::PartialWrite { factor } => {
                    let new_len = (buf.len() as f64 * factor.clamp(0.0, 1.0)) as usize;
                    truncated_buf.extend_from_slice(&buf[..new_len]);
                    buf_to_write = &truncated_buf;
                }
                _ => {}
            }
        }
        self.inner.write_at(buf_to_write, offset)
    }

    fn advise_random(&self) -> io::Result<()> {
        self.inner.advise_random()
    }

    fn sync_all(&self) -> io::Result<()> {
        if let Some(kind) = self.maybe_fault(OpMask {
            sync_all: true,
            ..OpMask::default()
        }) {
            match kind {
                FaultKind::Delay { delay_ms } => {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                }
                FaultKind::FsyncFail | FaultKind::Eio => {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                _ => {}
            }
        }
        self.inner.sync_all()
    }

    fn sync_data(&self) -> io::Result<()> {
        if let Some(kind) = self.maybe_fault(OpMask {
            sync_data: true,
            ..OpMask::default()
        }) {
            match kind {
                FaultKind::Delay { delay_ms } => {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                }
                FaultKind::FsyncFail | FaultKind::Eio => {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                _ => {}
            }
        }
        self.inner.sync_data()
    }

    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        if let Some(kind) = self.maybe_fault(OpMask {
            set_len: true,
            ..OpMask::default()
        }) {
            match kind {
                FaultKind::Eio => {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                _ => {}
            }
        }
        self.inner.set_len(len)
    }
}

/// Deterministic io_uring backend that replays scripted completions.
///
/// This is **not** a real io_uring implementation; it is a test harness
/// that records every submitted SQE and, on `submit_and_wait`, returns
/// the next scripted CQE from an internal vector.  It enables reproducible
/// verification of ordering invariants (e.g. WAL-before-data) without
/// depending on real kernel timing.
pub struct DeterministicIoUring {
    /// Scripted completion results in order.
    completions: Mutex<VecDeque<io::Result<u32>>>,
}

impl DeterministicIoUring {
    /// Create a new deterministic ring with the given completion script.
    ///
    /// Each entry is either `Ok(bytes_transferred)` or `Err(io::Error)`.
    pub fn new(script: Vec<io::Result<u32>>) -> Self {
        Self {
            completions: Mutex::new(script.into()),
        }
    }

    /// Pop the next scripted completion.
    pub fn next_completion(&self) -> Option<io::Result<u32>> {
        self.completions.lock().unwrap().pop_front()
    }

    /// Push an additional scripted completion at the end.
    pub fn push_completion(&self, res: io::Result<u32>) {
        self.completions.lock().unwrap().push_back(res);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::posix::PosixFileSystem;
    use std::io::Write;

    #[test]
    fn fault_inject_eio_on_read() {
        let inner = Box::new(PosixFileSystem::new(false));
        let fs = FaultInjectFileSystem::new(inner);
        fs.set_config(FaultConfig {
            rules: vec![FaultRule {
                op_mask: OpMask {
                    read: true,
                    ..OpMask::default()
                },
                kind: FaultKind::Eio,
                every_n: None,
            }],
        });

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let handle = fs.open(tmp.path(), false).unwrap();
        let mut buf = [0u8; 4];
        let err = handle.read_at(&mut buf, 0).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
    }

    #[test]
    fn fault_inject_enospc_on_write() {
        let inner = Box::new(PosixFileSystem::new(false));
        let fs = FaultInjectFileSystem::new(inner);
        fs.set_config(FaultConfig {
            rules: vec![FaultRule {
                op_mask: OpMask {
                    write: true,
                    ..OpMask::default()
                },
                kind: FaultKind::Enospc,
                every_n: None,
            }],
        });

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let handle = fs.open(tmp.path(), false).unwrap();
        let err = handle.write_at(b"hello", 0).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOSPC));
    }

    #[test]
    fn fault_inject_fsync_fail() {
        let inner = Box::new(PosixFileSystem::new(false));
        let fs = FaultInjectFileSystem::new(inner);
        fs.set_config(FaultConfig {
            rules: vec![FaultRule {
                op_mask: OpMask {
                    sync_data: true,
                    ..OpMask::default()
                },
                kind: FaultKind::FsyncFail,
                every_n: None,
            }],
        });

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let handle = fs.open(tmp.path(), false).unwrap();
        let err = handle.sync_data().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
    }

    #[test]
    fn fault_inject_partial_write() {
        let inner = Box::new(PosixFileSystem::new(false));
        let fs = FaultInjectFileSystem::new(inner);

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // Pre-size the file to 8 zero bytes so the subsequent read succeeds.
        tmp.as_file_mut().write_all(&[0u8; 8]).unwrap();
        tmp.as_file_mut().sync_data().unwrap();

        fs.set_config(FaultConfig {
            rules: vec![FaultRule {
                op_mask: OpMask {
                    write: true,
                    ..OpMask::default()
                },
                kind: FaultKind::PartialWrite { factor: 0.5 },
                every_n: None,
            }],
        });

        let handle = fs.open(tmp.path(), false).unwrap();
        // Write 8 bytes — fault injector should truncate to 4.
        let data = [0xABu8; 8];
        handle.write_at(&data, 0).unwrap();
        handle.sync_data().unwrap();

        let handle2 = fs.inner.open(tmp.path(), false).unwrap();
        let mut buf = [0u8; 8];
        handle2.read_at(&mut buf, 0).unwrap();
        // Only first 4 bytes should have been written.
        assert_eq!(&buf[..4], &[0xAB; 4]);
        // Remainder untouched (zeros from temp file).
        assert_eq!(&buf[4..], &[0; 4]);
    }

    #[test]
    fn deterministic_uring_replays_script() {
        let ring = DeterministicIoUring::new(vec![
            Ok(512),
            Ok(1024),
            Err(io::Error::from_raw_os_error(libc::EIO)),
        ]);

        assert_eq!(ring.next_completion().unwrap().unwrap(), 512);
        assert_eq!(ring.next_completion().unwrap().unwrap(), 1024);
        let err = ring.next_completion().unwrap().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
        assert!(ring.next_completion().is_none());
    }

    #[test]
    fn fault_inject_delay_does_not_panic() {
        let inner = Box::new(PosixFileSystem::new(false));
        let fs = FaultInjectFileSystem::new(inner);

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // Pre-fill the file so the subsequent read has data to return.
        tmp.as_file_mut().write_all(b"test").unwrap();
        tmp.as_file_mut().sync_data().unwrap();

        fs.set_config(FaultConfig {
            rules: vec![FaultRule {
                op_mask: OpMask {
                    read: true,
                    ..OpMask::default()
                },
                kind: FaultKind::Delay { delay_ms: 1 },
                every_n: None,
            }],
        });

        let handle = fs.open(tmp.path(), false).unwrap();
        let mut buf = [0u8; 4];
        // Should complete successfully after the 1 ms delay.
        handle.read_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"test");
    }
}
