use std::sync::Arc;

use crate::shared::PoolMapping;

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
