//! `wl_data_offer` protocol handler.
//!
//! A data offer represents content offered by another client via the
//! clipboard or drag-and-drop. Stub implementation.

use std::os::fd::OwnedFd;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;

// Request opcodes
const ACCEPT: u16 = 0;
pub const RECEIVE: u16 = 1;
const DESTROY: u16 = 2;
const FINISH: u16 = 3;
const SET_ACTIONS: u16 = 4;

/// `_fds` carries the pipe passed with `receive`. Dropping it closes our end,
/// which gives the requesting client an immediate EOF rather than a hang —
/// the right behaviour until the clipboard is actually implemented.
pub fn handle(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
    _fds: Vec<OwnedFd>,
) {
    match msg.message.op_code {
        ACCEPT | RECEIVE | FINISH | SET_ACTIONS => {
            // Not yet implemented
        }
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        _ => super::unknown_request(state, msg, "wl_data_offer"),
    }
}
