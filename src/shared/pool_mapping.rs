//! Client buffer memory, shared with a backend.

use super::shm_guard;
use std::os::unix::io::RawFd;
use tracing::warn;

/// A live `mmap` of a client's shm pool, unmapped when the last user drops it.
pub struct PoolMapping {
    /// A C pointer to the shared memory pool
    ptr: *mut libc::c_void,
    /// The size of the pool
    size: usize,
    /// Slot in the `SIGBUS` net covering this mapping, if one was free.
    guard_slot: Option<usize>,
}
unsafe impl Send for PoolMapping {}
unsafe impl Sync for PoolMapping {}

impl PoolMapping {
    /// Map a pool's file, or return `None` if it cannot safely be mapped.
    pub fn new(fd: RawFd, size: u32) -> Option<Self> {
        if let Err(e) = shm_guard::prepare_pool_file(fd, size) {
            warn!("refusing to map shm pool: {e}");
            return None;
        }

        let size = size as usize;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        Some(Self {
            ptr,
            size,
            guard_slot: shm_guard::register(ptr, size),
        })
    }

    /// Size accessor
    pub fn size(&self) -> usize {
        self.size
    }

    /// Borrow `len` bytes starting at `offset`.
    pub unsafe fn slice(&self, offset: usize, len: usize) -> Option<&[u8]> {
        if offset.checked_add(len)? > self.size {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>().add(offset), len) })
    }
}

impl Drop for PoolMapping {
    /// Cleans up a pool by unregistering the guard and munmapping the pool
    fn drop(&mut self) {
        if let Some(slot) = self.guard_slot {
            shm_guard::unregister(slot);
        }
        unsafe { libc::munmap(self.ptr, self.size) };
    }
}

impl std::fmt::Debug for PoolMapping {
    /// Debug print of the pool mapping
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolMapping")
            .field("ptr", &self.ptr)
            .field("size", &self.size)
            .field("guard_slot", &self.guard_slot)
            .finish()
    }
}
