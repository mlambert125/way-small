//! `xdg_system_bell` protocol handler.
//!
//! Enables clients to ring the system bell

use tracing::{debug, info, warn};

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;

// Request opcodes
const DESTROY: u16 = 0;
const RING: u16 = 1;

pub fn handle(_state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            debug!("xdg_system_bell: destroy");
        }
        RING => {
            info!("xdg_system_bell: ring");
        }
        op => {
            warn!("xdg_system_bell: unhandled opcode {}", op);
        }
    }
}
