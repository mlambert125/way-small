//! `wl_data_device` protocol handler.
//!
//! A data device receives selection (clipboard) and drag-and-drop events.
//! Stub implementation — accepts requests without acting on them.

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;

// Request opcodes
const START_DRAG: u16 = 0;
const SET_SELECTION: u16 = 1;
const RELEASE: u16 = 2;

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        START_DRAG | SET_SELECTION => {
            // Clipboard selection not implemented yet
        }
        RELEASE => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        _ => super::unknown_request(state, msg, "wl_data_device"),
    }
}
