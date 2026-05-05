//! wl_compositor protocol handler.
//!
//! The compositor global allows clients to create surfaces and regions.
//! Surfaces are the fundamental drawing primitive in Wayland — clients
//! attach buffers to them and commit to make content visible.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire::ArgReader;
use super::ObjectType;

// Request opcodes
const CREATE_SURFACE: u16 = 0;
const CREATE_REGION: u16 = 1;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        CREATE_SURFACE => handle_create_surface(state, msg).await,
        CREATE_REGION => handle_create_region(state, msg).await,
        op => {
            tracing::warn!("wl_compositor: unhandled opcode {}", op);
        }
    }
}

async fn handle_create_surface(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(surface_id) = args.new_id() else {
        let client = state.clients.get_or_create(msg.client_id);
        client
            .send_error(msg.message.object_id, 0, "wl_compositor.create_surface: malformed args")
            .await;
        return;
    };

    debug!("wl_compositor.create_surface: surface_id={}", surface_id);

    let client = state.clients.get_or_create(msg.client_id);
    client.register(surface_id, ObjectType::WlSurface);
    state.create_surface(surface_id, msg.client_id);
}

async fn handle_create_region(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(region_id) = args.new_id() else {
        let client = state.clients.get_or_create(msg.client_id);
        client
            .send_error(msg.message.object_id, 0, "wl_compositor.create_region: malformed args")
            .await;
        return;
    };

    debug!("wl_compositor.create_region: region_id={}", region_id);

    let client = state.clients.get_or_create(msg.client_id);
    client.register(region_id, ObjectType::WlRegion);
    state.create_region(region_id, msg.client_id);
}
