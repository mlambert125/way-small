//! `wl_subcompositor` protocol handler.
//!
//! The subcompositor global lets clients create subsurfaces — child surfaces
//! positioned relative to a parent and composited together with it.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::state::CompositorState;
use super::wire_utils::ArgReader;

#[cfg(test)]
mod tests;

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
        _ => super::unknown_request(state, msg, "wl_subcompositor"),
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

    if state.dnd_icon_surfaces.contains(&(client_id, surface_id)) {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_subcompositor.get_subsurface: surface already has drag icon role",
        );
        return;
    }

    debug!(
        "wl_subcompositor.get_subsurface: subsurface_id={} surface_id={} parent_id={}",
        subsurface_id, surface_id, parent_id
    );

    // A surface may not end up its own ancestor. The tree is walked to find a
    // surface's global position and recursed to hit-test and compose it, so a
    // cycle is not a wrong answer but a hang or a blown stack — and it takes
    // every other client with it. Two requests is all it costs a client to
    // build one, so the check is here rather than left to the walkers.
    if is_ancestor(state, client_id, surface_id, parent_id) {
        if let Some(client) = state.clients.get(client_id) {
            // wl_subcompositor.error.bad_parent = 1
            client.send_error(
                msg.message.object_id,
                1,
                "wl_subcompositor.get_subsurface: parent is a descendant of the surface",
            );
        }
        return;
    }

    let Some(client) = state.clients.get(client_id) else {
        return;
    };

    // Register before touching any surface state: a rejected id must not leave
    // a half-built parent-child relationship behind.
    let version = client.version(msg.message.object_id);
    if client
        .register_with_version(subsurface_id, ObjectType::WlSubsurface, version)
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

/// Whether `surface_id` is `candidate_id` or one of its ancestors.
///
/// Bounded by the parent chain, which is acyclic precisely because this runs
/// before every link is added.
fn is_ancestor(
    state: &CompositorState,
    client_id: u32,
    surface_id: u32,
    candidate_id: u32,
) -> bool {
    let mut current = candidate_id;
    loop {
        if current == surface_id {
            return true;
        }
        let Some(surface) = state.surfaces.get(&(client_id, current)) else {
            return false;
        };
        let Some(parent) = surface.parent else {
            return false;
        };
        current = parent;
    }
}
