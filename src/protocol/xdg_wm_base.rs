//! `xdg_wm_base` protocol handler.
//!
//! The `xdg_wm_base` global lets clients create `xdg_surfaces` (window-managed
//! surfaces). Also handles the ping/pong liveness protocol.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::state::CompositorState;
use super::wire_utils::{ArgReader, ArgWriter, message};

// Request opcodes
const DESTROY: u16 = 0;
const CREATE_POSITIONER: u16 = 1;
const GET_XDG_SURFACE: u16 = 2;
const PONG: u16 = 3;

// Event opcodes
pub const PING: u16 = 0;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id).await;
            }
        }
        CREATE_POSITIONER => handle_create_positioner(state, msg).await,
        GET_XDG_SURFACE => handle_get_xdg_surface(state, msg).await,
        PONG => handle_pong(msg),
        op => {
            tracing::warn!("xdg_wm_base: unhandled opcode {}", op);
        }
    }
}

async fn handle_create_positioner(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    let Some(positioner_id) = args.new_id() else {
        client
            .send_error(
                msg.message.object_id,
                0,
                "xdg_wm_base.create_positioner: malformed args",
            )
            .await;
        return;
    };

    debug!(
        "xdg_wm_base.create_positioner: positioner_id={}",
        positioner_id
    );

    client.register(positioner_id, ObjectType::XdgPositioner);
    state.create_xdg_positioner(msg.client_id, positioner_id);
}

async fn handle_get_xdg_surface(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    // get_xdg_surface args: new_id, object surface
    let (Some(xdg_surface_id), Some(wl_surface_id)) = (args.new_id(), args.u32()) else {
        client
            .send_error(
                msg.message.object_id,
                0,
                "xdg_wm_base.get_xdg_surface: malformed args",
            )
            .await;
        return;
    };

    debug!(
        "xdg_wm_base.get_xdg_surface: xdg_surface_id={} wl_surface_id={}",
        xdg_surface_id, wl_surface_id
    );

    if state
        .cursor_role_surfaces
        .contains(&(msg.client_id, wl_surface_id))
    {
        client
            .send_error(
                msg.message.object_id,
                0,
                "xdg_wm_base.get_xdg_surface: surface already has cursor role",
            )
            .await;
        return;
    }

    client.register(xdg_surface_id, ObjectType::XdgSurface);
    state.create_xdg_surface(msg.client_id, xdg_surface_id, wl_surface_id);
}

fn handle_pong(msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let serial = args.u32().unwrap_or(0);
    debug!("xdg_wm_base.pong: serial={}", serial);
}

/// Send a ping event to a client's `xdg_wm_base` object.
#[allow(dead_code)]
pub async fn send_ping(state: &mut CompositorState, client_id: u32, wm_base_id: u32, serial: u32) {
    let Some(client) = state.clients.get(client_id) else {
        tracing::warn!("Received message from unknown client {}", client_id);
        return;
    };
    let args = ArgWriter::new().u32(serial).build();
    let _ = client.send(message(wm_base_id, PING, args)).await;
}
