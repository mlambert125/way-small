//! Client buffer memory, shared with a backend.
//!
//! The compositor does not copy a client's pixels; a backend reads them where
//! they lie. These two types are what makes that safe: the mapping outlives the
//! pool object it came from, and the guard tells the compositor when nobody is
//! reading any more so `wl_buffer.release` may go out.

use std::os::unix::io::RawFd;
use std::sync::Arc;

use tracing::warn;

use super::shm_guard;

/// A live `mmap` of a client's shm pool, unmapped when the last user drops it.
///
/// Reference counted because the compositor is not the only reader: a
/// texture handed to the backend borrows these bytes directly rather than
/// copying them, so the mapping has to outlive the pool object it came from.
/// A resize or a pool destroy therefore replaces or forgets the `Arc` and lets
/// the last holder do the `munmap`.
pub struct PoolMapping {
    /// A C pointer to the shared memory pool
    ptr: *mut libc::c_void,
    /// The size of the pool
    size: usize,
    /// Slot in the `SIGBUS` net covering this mapping, if one was free.
    guard_slot: Option<usize>,
}

// SAFETY: the mapping is `PROT_READ` and never mutated through this type. The
// client can still write to the underlying file, which is what
// `wl_buffer.release` exists to coordinate; see `TextureImage::bytes`.
unsafe impl Send for PoolMapping {}
unsafe impl Sync for PoolMapping {}

impl PoolMapping {
    /// Map a pool's file, or return `None` if it cannot safely be mapped.
    ///
    /// The file is checked and sealed first — see
    /// [`shm_guard::prepare_pool_file`] — so a pool larger than its file is
    /// refused outright rather than becoming a page that faults on read.
    pub fn new(fd: RawFd, size: u32) -> Option<Self> {
        if let Err(e) = shm_guard::prepare_pool_file(fd, size) {
            warn!("refusing to map shm pool: {e}");
            return None;
        }

        let size = size as usize;
        // SAFETY: `fd` is the client's pool file and `size` has been checked
        // against the file's actual length.
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
    ///
    /// # Safety
    /// The client may write to these bytes at any time it is permitted to.
    /// See `TextureImage::bytes` for the invariant that makes reading them
    /// sound.
    pub unsafe fn slice(&self, offset: usize, len: usize) -> Option<&[u8]> {
        if offset.checked_add(len)? > self.size {
            return None;
        }
        // SAFETY: the range was just checked against the mapping's length.
        Some(unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>().add(offset), len) })
    }
}

impl Drop for PoolMapping {
    /// Cleans up a pool by unregistering the guard and munmapping the pool
    fn drop(&mut self) {
        // Leave the net before the mapping goes, so a fault on this range can
        // never be patched after the address has been handed back.
        if let Some(slot) = self.guard_slot {
            shm_guard::unregister(slot);
        }
        // SAFETY: this pointer came from `mmap` with this size and is unmapped
        // exactly once, when the last reference goes.
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

/// A per-buffer handle on the pool memory that buffer lives in.
///
/// This is what makes zero-copy safe. A texture handed to the backend borrows
/// the client's shm mapping instead of copying it, so `wl_buffer.release` — the
/// signal that the client may draw into the buffer again — must wait until
/// every reader has finished. Readers hold a clone of this, so the compositor
/// can tell they are finished by finding itself the only owner left.
///
/// Counting references rather than watching for a drop keeps the compositor's
/// own handle in place for as long as the buffer exists, so a client that
/// re-attaches a buffer it has not been told about yet still renders.
#[derive(Debug)]
pub struct BufferGuard {
    /// Reference counted pool mapping so that the backend and compositor
    /// can share this
    mapping: Arc<PoolMapping>,
}

impl BufferGuard {
    /// Take a handle on a pool's memory for one buffer living in it.
    pub fn new(mapping: Arc<PoolMapping>) -> Self {
        Self { mapping }
    }

    /// Accessor for retrieving a mapping
    pub fn mapping(&self) -> &PoolMapping {
        &self.mapping
    }
}
