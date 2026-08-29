//! `wl_shm_pool` protocol handler.
//!
//! A pool represents an mmap-able region of shared memory. Clients create
//! buffers from pools (specifying offset, dimensions, stride, format),
//! resize pools, and destroy them when done.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::state::CompositorState;
use super::wire_utils::ArgReader;

// Request opcodes
const CREATE_BUFFER: u16 = 0;
const DESTROY: u16 = 1;
const RESIZE: u16 = 2;

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        CREATE_BUFFER => handle_create_buffer(state, msg),
        DESTROY => handle_destroy(state, msg),
        RESIZE => handle_resize(state, msg),
        op => {
            tracing::warn!("wl_shm_pool: unhandled opcode {}", op);
        }
    }
}

fn handle_create_buffer(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    // create_buffer args: new_id, int32 offset, int32 width, int32 height, int32 stride, uint32 format
    let (Some(buffer_id), Some(offset), Some(width), Some(height), Some(stride), Some(format)) = (
        args.new_id(),
        args.i32(),
        args.i32(),
        args.i32(),
        args.i32(),
        args.u32(),
    ) else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_shm_pool.create_buffer: malformed args",
        );
        return;
    };

    debug!(
        "wl_shm_pool.create_buffer: buffer_id={} offset={} {}x{} stride={} format={}",
        buffer_id, offset, width, height, stride, format
    );

    if client.register(buffer_id, ObjectType::WlBuffer).is_err() {
        return;
    }
    state.register_buffer(
        msg.client_id,
        buffer_id,
        msg.message.object_id,
        offset,
        width,
        height,
        stride,
        format,
    );
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let pool_id = msg.message.object_id;
    debug!("wl_shm_pool.destroy: pool_id={}", pool_id);
    state.destroy_shm_pool(msg.client_id, pool_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(pool_id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

fn handle_resize(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(new_size) = args.i32() else {
        return;
    };
    let pool_id = msg.message.object_id;
    debug!(
        "wl_shm_pool.resize: pool_id={} new_size={}",
        pool_id, new_size
    );
    state.resize_shm_pool(msg.client_id, pool_id, new_size.unsigned_abs());
}
