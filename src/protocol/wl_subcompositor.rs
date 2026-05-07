//! wl_subcompositor protocol handler.
//!
//! The subcompositor global lets clients create subsurfaces — child surfaces
//! positioned relative to a parent and composited together with it.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire::ArgReader;
use super::ObjectType;

// Request opcodes
const DESTROY: u16 = 0;
const GET_SUBSURFACE: u16 = 1;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            let client = state.clients.get_or_create(msg.client_id);
            client.unregister(msg.message.object_id).await;
        }
        GET_SUBSURFACE => handle_get_subsurface(state, msg).await,
        op => {
            tracing::warn!("wl_subcompositor: unhandled opcode {}", op);
        }
    }
}

async fn handle_get_subsurface(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let mut args = ArgReader::new(&msg.message.args);
    // get_subsurface args: new_id, object surface, object parent
    let (Some(subsurface_id), Some(surface_id), Some(parent_id)) =
        (args.new_id(), args.u32(), args.u32())
    else {
        let client = state.clients.get_or_create(msg.client_id);
        client
            .send_error(
                msg.message.object_id,
                0,
                "wl_subcompositor.get_subsurface: malformed args",
            )
            .await;
        return;
    };

    let client_id = msg.client_id;
    debug!(
        "wl_subcompositor.get_subsurface: subsurface_id={} surface_id={} parent_id={}",
        subsurface_id, surface_id, parent_id
    );

    // Set up the parent-child relationship
    if let Some(surface) = state.surfaces.get_mut(&(client_id, surface_id)) {
        surface.parent = Some(parent_id);
    }
    if let Some(parent) = state.surfaces.get_mut(&(client_id, parent_id)) {
        parent.children.push(surface_id);
    }

    let client = state.clients.get_or_create(client_id);
    client.register(subsurface_id, ObjectType::WlSubsurface);

    // Store the mapping from subsurface object id to the wl_surface id it controls
    state.subsurface_map.insert((client_id, subsurface_id), surface_id);
}
