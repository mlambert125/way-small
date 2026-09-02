//! `xdg_system_bell` protocol handler.
//!
//! Enables clients to ring the system bell

use tracing::{debug, info};

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;

// Request opcodes
const DESTROY: u16 = 0;
const RING: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            debug!("xdg_system_bell: destroy");
        }
        RING => {
            info!("xdg_system_bell: ring");
        }
        _ => super::unknown_request(state, msg, "xdg_system_bell_v1"),
    }
}
