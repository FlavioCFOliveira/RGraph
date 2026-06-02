use std::alloc::{alloc, dealloc, Layout};
use std::ops::{Deref, DerefMut};
use std::slice;

/// A page-aligned buffer owned by the caller.
///
/// The backing memory is allocated with **4 KiB alignment** so it can be
/// passed directly to `O_DIRECT` file I/O and `io_uring` without kernel
/// copying.
///
/// # Safety
///
/// The buffer is `Send` and `Sync` because the allocation is plain
/// page-aligned memory with no aliasing.
#[derive(Debug)]
pub struct AlignedBuffer {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

impl AlignedBuffer {
    /// Minimum alignment required by the kernel for direct I/O.
    pub const ALIGNMENT: usize = 4096;

    /// Allocate a zeroed buffer of `size` bytes aligned to [`Self::ALIGNMENT`].
    pub fn zeroed(size: usize) -> Self {
        let layout =
            Layout::from_size_align(size, Self::ALIGNMENT).expect("valid aligned layout");
        // SAFETY: layout is non-zero and alignment is a power of two.
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        // SAFETY: we own `ptr..ptr+size` and it is valid for writes.
        unsafe { std::ptr::write_bytes(ptr, 0, size) };
        Self { ptr, len: size, cap: size }
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Is the buffer empty?
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Resize the buffer to `new_len`.  If `new_len` > `self.len`, the new
    /// bytes are zeroed.
    pub fn resize(&mut self, new_len: usize) {
        if new_len == self.len {
            return;
        }
        if new_len <= self.cap {
            if new_len > self.len {
                // SAFETY: `self.ptr + self.len .. self.ptr + new_len` is valid
                unsafe { std::ptr::write_bytes(self.ptr.add(self.len), 0, new_len - self.len) };
            }
            self.len = new_len;
            return;
        }
        let new = Self::zeroed(new_len);
        let copy_len = self.len.min(new_len);
        // SAFETY: both pointers are valid for `copy_len` bytes.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr, new.ptr, copy_len) };
        *self = new;
    }
}

impl Clone for AlignedBuffer {
    fn clone(&self) -> Self {
        let mut new = Self::zeroed(self.len);
        new.copy_from_slice(&self[..]);
        new
    }
}

// SAFETY: `AlignedBuffer` owns its memory and there is no aliasing.
unsafe impl Send for AlignedBuffer {}
unsafe impl Sync for AlignedBuffer {}

impl Deref for AlignedBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: `ptr` is valid for `len` bytes and we own it exclusively.
        unsafe { slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for AlignedBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `ptr` is valid for `len` bytes and we own it exclusively.
        unsafe { slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        let layout =
            Layout::from_size_align(self.cap, Self::ALIGNMENT).expect("valid aligned layout");
        // SAFETY: `ptr` was allocated with exactly this layout.
        unsafe { dealloc(self.ptr, layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_is_aligned() {
        let buf = AlignedBuffer::zeroed(8192);
        assert_eq!(buf.len(), 8192);
        assert_eq!(buf.as_ptr() as usize % AlignedBuffer::ALIGNMENT, 0);
    }

    #[test]
    fn zeroed_buffer_is_all_zeros() {
        let buf = AlignedBuffer::zeroed(8192);
        assert!(buf.iter().all(|b| *b == 0));
    }

    #[test]
    fn resize_grows_and_zeros() {
        let mut buf = AlignedBuffer::zeroed(4096);
        buf[0] = 1;
        buf.resize(8192);
        assert_eq!(buf.len(), 8192);
        assert_eq!(buf[0], 1);
        assert!(buf[4096..].iter().all(|b| *b == 0));
    }

    #[test]
    fn resize_shrinks() {
        let mut buf = AlignedBuffer::zeroed(8192);
        buf.resize(4096);
        assert_eq!(buf.len(), 4096);
    }
}
