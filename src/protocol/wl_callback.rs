//! wl_callback protocol handler.
//!
//! Callbacks are one-shot objects created by the compositor (e.g. for
//! wl_display.sync or wl_surface.frame). Clients should never send
//! requests to them — any request is a protocol error.

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

pub fn handle(msg: &WaylandProtocolMessageWithClientInfo) {
    tracing::warn!(
        "client {}: unexpected request on wl_callback id={}",
        msg.client_id,
        msg.message.object_id,
    );
}
