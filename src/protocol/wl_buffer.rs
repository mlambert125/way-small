//! wl_buffer protocol handler.
//!
//! A buffer represents pixel data that can be attached to a surface.
//! The only client request is destroy. The compositor sends the release
//! event when it's done reading the buffer's contents.

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;

// Request opcodes
const DESTROY: u16 = 0;

// Event opcodes (sent by compositor)
pub const RELEASE: u16 = 0;

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            let buffer_id = msg.message.object_id;
            state.destroy_buffer(buffer_id);
            let client = state.clients.get_or_create(msg.client_id);
            client.unregister(buffer_id);
        }
        op => {
            tracing::warn!("wl_buffer: unhandled opcode {}", op);
        }
    }
}
