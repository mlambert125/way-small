//! wl_shm protocol handler.
//!
//! Manages shared memory for buffer allocation. Clients bind wl_shm to
//! discover supported pixel formats, then call create_pool with a file
//! descriptor to an mmap-able region. Pools are used to allocate wl_buffers.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::client::ClientState;
use super::state::CompositorState;
use super::wire::{ArgReader, ArgWriter, message};

// Request opcodes
const CREATE_POOL: u16 = 0;

// Event opcodes
const FORMAT: u16 = 0;

// Pixel format constants (from wayland-protocol's wl_shm.xml)
const FORMAT_ARGB8888: u32 = 0;
const FORMAT_XRGB8888: u32 = 1;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        CREATE_POOL => handle_create_pool(state, msg).await,
        op => {
            tracing::warn!("wl_shm: unhandled opcode {}", op);
        }
    }
}

/// Send wl_shm.format events for all supported pixel formats.
pub async fn send_formats(client: &mut ClientState, shm_id: u32) {
    for &fmt in &[FORMAT_ARGB8888, FORMAT_XRGB8888] {
        let args = ArgWriter::new().u32(fmt).build();
        if client.send(message(shm_id, FORMAT, args)).await.is_err() {
            return;
        }
    }
}

async fn handle_create_pool(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let mut args = ArgReader::new(&msg.message.args);
    // create_pool args: new_id, fd (passed out-of-band), int32 size
    let (Some(pool_id), Some(size)) = (args.new_id(), args.i32()) else {
        let client = state.clients.get_or_create(msg.client_id);
        client
            .send_error(
                msg.message.object_id,
                0,
                "wl_shm.create_pool: malformed args",
            )
            .await;
        return;
    };

    let fd = msg.message.fd_queue.lock().unwrap().pop_front();

    // The FD is the first one attached to this message
    let Some(fd) = fd else {
        let client = state.clients.get_or_create(msg.client_id);
        client
            .send_error(msg.message.object_id, 0, "wl_shm.create_pool: missing fd")
            .await;
        return;
    };

    if size <= 0 {
        let client = state.clients.get_or_create(msg.client_id);
        client
            .send_error(msg.message.object_id, 0, "wl_shm.create_pool: invalid size")
            .await;
        return;
    }

    debug!(
        "wl_shm.create_pool: pool_id={} fd={} size={}",
        pool_id, fd, size
    );

    let client = state.clients.get_or_create(msg.client_id);
    client.register(pool_id, ObjectType::WlShmPool);
    state.register_shm_pool(pool_id, msg.client_id, fd, size as u32);
}
