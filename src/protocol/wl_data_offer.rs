//! wl_data_offer protocol handler.
//!
//! A data offer represents content offered by another client via the
//! clipboard or drag-and-drop. Stub implementation.

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;

// Request opcodes
const ACCEPT: u16 = 0;
const RECEIVE: u16 = 1;
const DESTROY: u16 = 2;
const FINISH: u16 = 3;
const SET_ACTIONS: u16 = 4;

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        ACCEPT | RECEIVE | FINISH | SET_ACTIONS => {
            // Not yet implemented
        }
        DESTROY => {
            let client = state.clients.get_or_create(msg.client_id);
            client.unregister(msg.message.object_id);
        }
        op => {
            tracing::warn!("wl_data_offer: unhandled opcode {}", op);
        }
    }
}
