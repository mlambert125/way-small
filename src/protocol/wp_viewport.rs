//! wp_viewport protocol handler.
//!
//! Handles set_source (crop rectangle) and set_destination (output size)
//! requests. Values are stored as pending state and applied on the next
//! wl_surface.commit, matching the double-buffered semantics of the protocol.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire::ArgReader;

// wp_viewport request opcodes
const DESTROY: u16 = 0;
const SET_SOURCE: u16 = 1;
const SET_DESTINATION: u16 = 2;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        SET_SOURCE => handle_set_source(state, msg),
        SET_DESTINATION => handle_set_destination(state, msg),
        op => {
            tracing::warn!("wp_viewport: unhandled opcode {}", op);
        }
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let viewport_id = msg.message.object_id;
    debug!("wp_viewport.destroy: viewport_id={}", viewport_id);
    state.destroy_viewport(msg.client_id, viewport_id);
    let client = state.clients.get_or_create(msg.client_id);
    client.unregister(viewport_id);
}

fn handle_set_source(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // All four args are wl_fixed_t (24.8 fixed-point)
    let (Some(x), Some(y), Some(width), Some(height)) =
        (args.fixed(), args.fixed(), args.fixed(), args.fixed())
    else {
        return;
    };

    let viewport_id = msg.message.object_id;
    debug!(
        "wp_viewport.set_source: viewport_id={} src=({}, {}, {}, {})",
        viewport_id, x, y, width, height
    );

    if let Some(vp) = state.viewports.get_mut(&(msg.client_id, viewport_id)) {
        // -1.0 means "unset" per protocol
        if x == -1.0 && y == -1.0 && width == -1.0 && height == -1.0 {
            vp.pending_source = Some(None);
        } else {
            vp.pending_source = Some(Some((x, y, width, height)));
        }
    }
}

fn handle_set_destination(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(width), Some(height)) = (args.i32(), args.i32()) else {
        return;
    };

    let viewport_id = msg.message.object_id;
    debug!(
        "wp_viewport.set_destination: viewport_id={} dst=({}, {})",
        viewport_id, width, height
    );

    if let Some(vp) = state.viewports.get_mut(&(msg.client_id, viewport_id)) {
        // -1 means "unset" per protocol
        if width == -1 && height == -1 {
            vp.pending_destination = Some(None);
        } else {
            vp.pending_destination = Some(Some((width, height)));
        }
    }
}
