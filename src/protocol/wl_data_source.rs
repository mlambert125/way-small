//! wl_data_source protocol handler.
//!
//! A data source offers content for clipboard or drag-and-drop.
//! Stub implementation — accepts mime type offers without acting on them.

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;

// Request opcodes
const OFFER: u16 = 0;
const DESTROY: u16 = 1;
const SET_ACTIONS: u16 = 2;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        OFFER => {
            // Client is advertising a mime type it can provide — ignored for now
        }
        DESTROY => {
            let client = state.clients.get_or_create(msg.client_id);
            client.unregister(msg.message.object_id).await;
        }
        SET_ACTIONS => {
            // DnD actions — ignored for now
        }
        op => {
            tracing::warn!("wl_data_source: unhandled opcode {}", op);
        }
    }
}
