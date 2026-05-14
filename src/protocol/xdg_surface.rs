//! xdg_surface protocol handler.
//!
//! An xdg_surface wraps a wl_surface and adds window-management semantics.
//! Clients assign a role (toplevel or popup) and must ack configure events
//! before committing content.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::state::CompositorState;
use super::wire_utils::{ArgReader, ArgWriter, message};

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
        DESTROY => handle_destroy(state, msg).await,
        GET_TOPLEVEL => handle_get_toplevel(state, msg).await,
        GET_POPUP => handle_get_popup(state, msg).await,
        SET_WINDOW_GEOMETRY => handle_set_window_geometry(state, msg),
        ACK_CONFIGURE => handle_ack_configure(state, msg),
        op => {
            tracing::warn!("xdg_surface: unhandled opcode {}", op);
        }
    }
}

async fn handle_destroy(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let xdg_surface_id = msg.message.object_id;
    debug!("xdg_surface.destroy: xdg_surface_id={}", xdg_surface_id);
    state.destroy_xdg_surface(msg.client_id, xdg_surface_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(xdg_surface_id).await;
    }
}

async fn handle_get_toplevel(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    let Some(toplevel_id) = args.new_id() else {
        client
            .send_error(
                msg.message.object_id,
                0,
                "xdg_surface.get_toplevel: malformed args",
            )
            .await;
        return;
    };

    let xdg_surface_id = msg.message.object_id;
    debug!(
        "xdg_surface.get_toplevel: toplevel_id={} xdg_surface_id={}",
        toplevel_id, xdg_surface_id
    );

    let client_id = msg.client_id;
    client.register(toplevel_id, ObjectType::XdgToplevel);
    state.create_xdg_toplevel(client_id, toplevel_id, xdg_surface_id);

    // Send initial xdg_toplevel.configure (empty states, 0x0 = client picks size)
    super::xdg_toplevel::send_configure(state, client_id, toplevel_id, 0, 0).await;

    // Send xdg_surface.configure with a serial the client must ack
    let serial = super::next_serial();
    let args = ArgWriter::new().u32(serial).build();

    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let _ = client.send(message(xdg_surface_id, CONFIGURE, args)).await;

    // Take focus on the new toplevel
    let wl_surface_id = state
        .xdg_surfaces
        .get(&(client_id, xdg_surface_id))
        .map(|s| s.wl_surface_id);
    if let Some(wl_surface_id) = wl_surface_id {
        crate::compositor::switch_focus(state, (client_id, wl_surface_id)).await;
    }
}

async fn handle_get_popup(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    // get_popup args: new_id popup, object parent (xdg_surface), object positioner
    let (Some(popup_id), Some(parent_xdg_surface_id), Some(positioner_id)) =
        (args.new_id(), args.u32(), args.u32())
    else {
        client
            .send_error(
                msg.message.object_id,
                0,
                "xdg_surface.get_popup: malformed args",
            )
            .await;
        return;
    };

    let xdg_surface_id = msg.message.object_id;
    let client_id = msg.client_id;
    debug!(
        "xdg_surface.get_popup: popup_id={} parent={} positioner={}",
        popup_id, parent_xdg_surface_id, positioner_id
    );

    // Compute position from positioner
    let (x, y, width, height) =
        if let Some(pos) = state.xdg_positioners.get(&(client_id, positioner_id)) {
            compute_popup_position(pos)
        } else {
            (0, 0, 1, 1)
        };

    client.register(popup_id, ObjectType::XdgPopup);
    state.create_xdg_popup(
        client_id,
        popup_id,
        xdg_surface_id,
        parent_xdg_surface_id,
        x,
        y,
        width,
        height,
    );

    // Parent the popup's wl_surface under the parent's wl_surface
    let popup_wl_surface = state
        .xdg_surfaces
        .get(&(client_id, xdg_surface_id))
        .map(|s| s.wl_surface_id);
    let parent_wl_surface = state
        .xdg_surfaces
        .get(&(client_id, parent_xdg_surface_id))
        .map(|s| s.wl_surface_id);

    if let (Some(popup_wl), Some(parent_wl)) = (popup_wl_surface, parent_wl_surface) {
        if let Some(surface) = state.surfaces.get_mut(&(client_id, popup_wl)) {
            surface.parent = Some(parent_wl);
            surface.subsurface_position = (x, y);
        }
        if let Some(parent) = state.surfaces.get_mut(&(client_id, parent_wl)) {
            parent.children.push(popup_wl);
        }
    }

    // Send xdg_popup.configure with the computed position
    super::xdg_popup::send_configure(state, client_id, popup_id, x, y, width, height).await;

    // Send xdg_surface.configure with a serial the client must ack
    let serial = super::next_serial();
    let configure_args = ArgWriter::new().u32(serial).build();
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let _ = client
        .send(message(xdg_surface_id, CONFIGURE, configure_args))
        .await;
}

fn handle_set_window_geometry(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
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
    if let Some(xdg_surface) = state.xdg_surfaces.get_mut(&(msg.client_id, xdg_surface_id)) {
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
    if let Some(xdg_surface) = state.xdg_surfaces.get_mut(&(msg.client_id, xdg_surface_id)) {
        xdg_surface.configured = true;
    }
}

/// Send an xdg_surface.configure event.
#[allow(dead_code)]
pub async fn send_configure(
    state: &mut CompositorState,
    client_id: u32,
    xdg_surface_id: u32,
    serial: u32,
) {
    let args = ArgWriter::new().u32(serial).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(xdg_surface_id, CONFIGURE, args)).await;
    }
}

/// Compute popup position from a positioner.
///
/// The anchor point is derived from the anchor_rect + anchor edge.
/// The popup is placed so that the gravity edge of the popup aligns
/// with the anchor point, then offset is applied.
fn compute_popup_position(pos: &super::state::XdgPositionerState) -> (i32, i32, i32, i32) {
    let (ar_x, ar_y, ar_w, ar_h) = pos.anchor_rect;

    // Anchor point within the anchor rect
    // Anchor edges: 0=none(center), 1=top, 2=bottom, 3=left, 4=right,
    //               5=top_left, 6=bottom_left, 7=top_right, 8=bottom_right
    let anchor_x = match pos.anchor {
        3 | 5 | 6 => ar_x,        // left edge
        4 | 7 | 8 => ar_x + ar_w, // right edge
        _ => ar_x + ar_w / 2,     // center
    };
    let anchor_y = match pos.anchor {
        1 | 5 | 7 => ar_y,        // top edge
        2 | 6 | 8 => ar_y + ar_h, // bottom edge
        _ => ar_y + ar_h / 2,     // center
    };

    // Gravity determines which part of the popup is placed at the anchor point
    // Same enum values as anchor
    let popup_x = match pos.gravity {
        3 | 5 | 6 => anchor_x - pos.width, // popup extends left
        4 | 7 | 8 => anchor_x,             // popup extends right
        _ => anchor_x - pos.width / 2,     // centered
    };
    let popup_y = match pos.gravity {
        1 | 5 | 7 => anchor_y - pos.height, // popup extends up
        2 | 6 | 8 => anchor_y,              // popup extends down
        _ => anchor_y - pos.height / 2,     // centered
    };

    (
        popup_x + pos.offset.0,
        popup_y + pos.offset.1,
        pos.width,
        pos.height,
    )
}
