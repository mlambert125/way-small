//! `wp_viewporter` protocol handler.
//!
//! Allows clients to crop (`set_source`) and scale (`set_destination`) surface
//! content. The `wp_viewporter` global creates `wp_viewport` objects bound to
//! a specific `wl_surface`. Source and destination are double-buffered and
//! applied on `wl_surface.commit`.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::state::CompositorState;
use super::wire_utils::ArgReader;

// wp_viewporter request opcodes
const DESTROY: u16 = 0;
const GET_VIEWPORT: u16 = 1;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id).await;
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        GET_VIEWPORT => handle_get_viewport(state, msg).await,
        op => {
            tracing::warn!("wp_viewporter: unhandled opcode {}", op);
        }
    }
}

async fn handle_get_viewport(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    let (Some(viewport_id), Some(surface_id)) = (args.new_id(), args.u32()) else {
        return;
    };

    debug!(
        "wp_viewporter.get_viewport: viewport_id={} surface_id={}",
        viewport_id, surface_id
    );

    // Check that the surface doesn't already have a viewport
    if state
        .surface_viewport
        .contains_key(&(msg.client_id, surface_id))
    {
        client
            .send_error(
                msg.message.object_id,
                0, // error::viewport_exists
                "surface already has a viewport",
            )
            .await;
        return;
    }

    client.register(viewport_id, ObjectType::WpViewport);

    state.create_viewport(msg.client_id, viewport_id, surface_id);
}
