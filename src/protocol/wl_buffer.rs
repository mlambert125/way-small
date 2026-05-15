//! `wl_buffer` protocol handler.
//!
//! A buffer represents pixel data that can be attached to a surface.
//! The only client request is destroy. The compositor sends the release
//! event when it's done reading the buffer's contents.

use crate::{protocol::message, wayland_socket::WaylandProtocolMessageWithClientInfo};

use super::state::CompositorState;

// Request opcodes
const DESTROY: u16 = 0;

// Event opcodes (sent by compositor)
pub const RELEASE: u16 = 0;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            let buffer_id = msg.message.object_id;
            state.destroy_buffer(msg.client_id, buffer_id);
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(buffer_id).await;
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        op => {
            tracing::warn!("wl_buffer: unhandled opcode {}", op);
        }
    }
}

#[allow(dead_code)]
pub async fn send_release(state: &mut CompositorState, client_id: u32, buffer_id: u32) {
    let Some(client) = state.clients.get(client_id) else {
        tracing::warn!("Received message from unknown client {}", client_id);
        return;
    };
    let _ = client.send(message(buffer_id, RELEASE, vec![])).await;
}
