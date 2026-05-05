//! xdg_surface protocol handler.
//!
//! An xdg_surface wraps a wl_surface and adds window-management semantics.
//! Clients assign a role (toplevel or popup) and must ack configure events
//! before committing content.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire::{ArgReader, ArgWriter, message};
use super::ObjectType;

// Request opcodes
const DESTROY: u16 = 0;
const GET_TOPLEVEL: u16 = 1;
const GET_POPUP: u16 = 2;
const SET_WINDOW_GEOMETRY: u16 = 3;
const ACK_CONFIGURE: u16 = 4;

// Event opcodes
pub const CONFIGURE: u16 = 0;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        GET_TOPLEVEL => handle_get_toplevel(state, msg).await,
        GET_POPUP => {
            tracing::warn!("xdg_surface.get_popup: not implemented yet");
        }
        SET_WINDOW_GEOMETRY => handle_set_window_geometry(state, msg),
        ACK_CONFIGURE => handle_ack_configure(state, msg),
        op => {
            tracing::warn!("xdg_surface: unhandled opcode {}", op);
        }
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let xdg_surface_id = msg.message.object_id;
    debug!("xdg_surface.destroy: xdg_surface_id={}", xdg_surface_id);
    state.destroy_xdg_surface(xdg_surface_id);
    let client = state.clients.get_or_create(msg.client_id);
    client.unregister(xdg_surface_id);
}

async fn handle_get_toplevel(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(toplevel_id) = args.new_id() else {
        let client = state.clients.get_or_create(msg.client_id);
        client
            .send_error(msg.message.object_id, 0, "xdg_surface.get_toplevel: malformed args")
            .await;
        return;
    };

    let xdg_surface_id = msg.message.object_id;
    debug!(
        "xdg_surface.get_toplevel: toplevel_id={} xdg_surface_id={}",
        toplevel_id, xdg_surface_id
    );

    let client = state.clients.get_or_create(msg.client_id);
    client.register(toplevel_id, ObjectType::XdgToplevel);
    state.create_xdg_toplevel(toplevel_id, msg.client_id, xdg_surface_id);

    // Send initial xdg_toplevel.configure (empty states, 0x0 = client picks size)
    super::xdg_toplevel::send_configure(state, msg.client_id, toplevel_id, 0, 0).await;

    // Send xdg_surface.configure with a serial the client must ack
    let serial = super::next_serial();
    let args = ArgWriter::new().u32(serial).build();
    let client = state.clients.get_or_create(msg.client_id);
    let _ = client.send(message(xdg_surface_id, CONFIGURE, args)).await;
}

fn handle_set_window_geometry(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y), Some(w), Some(h)) = (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        return;
    };
    let xdg_surface_id = msg.message.object_id;
    debug!(
        "xdg_surface.set_window_geometry: {}x{} at ({},{})",
        w, h, x, y
    );
    if let Some(xdg_surface) = state.xdg_surfaces.get_mut(&xdg_surface_id) {
        xdg_surface.geometry = Some((x, y, w, h));
    }
}

fn handle_ack_configure(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(serial) = args.u32() else {
        return;
    };
    let xdg_surface_id = msg.message.object_id;
    debug!(
        "xdg_surface.ack_configure: xdg_surface_id={} serial={}",
        xdg_surface_id, serial
    );
    if let Some(xdg_surface) = state.xdg_surfaces.get_mut(&xdg_surface_id) {
        xdg_surface.configured = true;
    }
}

/// Send an xdg_surface.configure event.
#[allow(dead_code)]
pub async fn send_configure(state: &mut CompositorState, client_id: u32, xdg_surface_id: u32, serial: u32) {
    let args = ArgWriter::new().u32(serial).build();
    let client = state.clients.get_or_create(client_id);
    let _ = client.send(message(xdg_surface_id, CONFIGURE, args)).await;
}
