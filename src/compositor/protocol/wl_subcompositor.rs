//! `wl_subcompositor` protocol handler.
//!
//! The subcompositor global lets clients create subsurfaces — child surfaces
//! positioned relative to a parent and composited together with it.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::state::CompositorState;
use super::wire_utils::ArgReader;

// Request opcodes
const DESTROY: u16 = 0;
const GET_SUBSURFACE: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        GET_SUBSURFACE => handle_get_subsurface(state, msg),
        op => {
            tracing::warn!("wl_subcompositor: unhandled opcode {}", op);
        }
    }
}

fn handle_get_subsurface(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    // get_subsurface args: new_id, object surface, object parent
    let (Some(subsurface_id), Some(surface_id), Some(parent_id)) =
        (args.new_id(), args.u32(), args.u32())
    else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_subcompositor.get_subsurface: malformed args",
        );
        return;
    };

    let client_id = msg.client_id;

    if state
        .cursor_role_surfaces
        .contains(&(client_id, surface_id))
    {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_subcompositor.get_subsurface: surface already has cursor role",
        );
        return;
    }

    debug!(
        "wl_subcompositor.get_subsurface: subsurface_id={} surface_id={} parent_id={}",
        subsurface_id, surface_id, parent_id
    );

    // Register before touching any surface state: a rejected id must not leave
    // a half-built parent-child relationship behind.
    if client
        .register(subsurface_id, ObjectType::WlSubsurface)
        .is_err()
    {
        return;
    }

    // Set up the parent-child relationship
    if let Some(surface) = state.surfaces.get_mut(&(client_id, surface_id)) {
        surface.parent = Some(parent_id);
    }
    if let Some(parent) = state.surfaces.get_mut(&(client_id, parent_id)) {
        parent.children.push(surface_id);
    }

    // Store the mapping from subsurface object id to the wl_surface id it controls
    state
        .subsurface_map
        .insert((client_id, subsurface_id), surface_id);
}
