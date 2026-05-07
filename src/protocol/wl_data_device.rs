//! wl_data_device protocol handler.
//!
//! A data device receives selection (clipboard) and drag-and-drop events.
//! Stub implementation — accepts requests without acting on them.

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;

// Request opcodes
const START_DRAG: u16 = 0;
const SET_SELECTION: u16 = 1;
const RELEASE: u16 = 2;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        START_DRAG => {
            // Drag-and-drop not implemented yet
        }
        SET_SELECTION => {
            // Clipboard selection not implemented yet
        }
        RELEASE => {
            let client = state.clients.get_or_create(msg.client_id);
            client.unregister(msg.message.object_id).await;
        }
        op => {
            tracing::warn!("wl_data_device: unhandled opcode {}", op);
        }
    }
}
