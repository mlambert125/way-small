//! `xdg_surface` protocol handler.
//!
//! An `xdg_surface` wraps a `wl_surface` and adds window-management semantics.
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

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        GET_TOPLEVEL => handle_get_toplevel(state, msg),
        GET_POPUP => handle_get_popup(state, msg),
        SET_WINDOW_GEOMETRY => handle_set_window_geometry(state, msg),
        ACK_CONFIGURE => handle_ack_configure(state, msg),
        _ => super::unknown_request(state, msg, "xdg_surface"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let xdg_surface_id = msg.message.object_id;
    debug!("xdg_surface.destroy: xdg_surface_id={}", xdg_surface_id);
    state.destroy_xdg_surface(msg.client_id, xdg_surface_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(xdg_surface_id);
    }
}

fn handle_get_toplevel(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    let Some(toplevel_id) = args.new_id() else {
        client.send_error(
            msg.message.object_id,
            0,
            "xdg_surface.get_toplevel: malformed args",
        );
        return;
    };

    let xdg_surface_id = msg.message.object_id;
    debug!(
        "xdg_surface.get_toplevel: toplevel_id={} xdg_surface_id={}",
        toplevel_id, xdg_surface_id
    );

    let client_id = msg.client_id;
    // The xdg_surface's version, carried down. Without it the toplevel sits at
    // version 1 and every version-gated event on it — `wm_capabilities` and
    // `configure_bounds` — is silently suppressed.
    let version = client.version(xdg_surface_id);
    if client
        .register_with_version(toplevel_id, ObjectType::XdgToplevel, version)
        .is_err()
    {
        return;
    }
    state.create_xdg_toplevel(client_id, toplevel_id, xdg_surface_id);

    // Before the first configure: the client decides what chrome to draw off the
    // back of it, and a configure it has already acted on is too late.
    super::xdg_toplevel::send_wm_capabilities(state, client_id, toplevel_id);

    // The initial configure. A zero size leaves the client to pick its own,
    // which is what it is for on a window that has never been mapped; the
    // bounds that go out with it say how much room there is to pick within.
    super::xdg_toplevel::configure(state, (client_id, toplevel_id), 0, 0);

    // Take focus on the new toplevel
    let wl_surface_id = state
        .xdg_surfaces
        .get(&(client_id, xdg_surface_id))
        .map(|s| s.wl_surface_id);
    if let Some(wl_surface_id) = wl_surface_id {
        crate::compositor::switch_focus(state, (client_id, wl_surface_id));
    }
}

fn handle_get_popup(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    // get_popup args: new_id popup, object parent (xdg_surface), object positioner
    let (Some(popup_id), Some(parent_xdg_surface_id), Some(positioner_id)) =
        (args.new_id(), args.u32(), args.u32())
    else {
        client.send_error(
            msg.message.object_id,
            0,
            "xdg_surface.get_popup: malformed args",
        );
        return;
    };

    let xdg_surface_id = msg.message.object_id;
    let client_id = msg.client_id;
    debug!(
        "xdg_surface.get_popup: popup_id={} parent={} positioner={}",
        popup_id, parent_xdg_surface_id, positioner_id
    );

    // Position from the positioner, kept on screen where the client asked for
    // that. Needs the parent's `wl_surface` because the constraining region is
    // the parent's output expressed relative to the parent.
    let parent_surface = state
        .xdg_surfaces
        .get(&(client_id, parent_xdg_surface_id))
        .map(|xdg| (client_id, xdg.wl_surface_id));
    let (x, y, width, height) = parent_surface
        .and_then(|parent| state.place_popup((client_id, positioner_id), parent))
        .unwrap_or((0, 0, 1, 1));

    // NLL: the borrow taken to decode the arguments ends above, because placing
    // the popup reads the outputs and the surface tree — state the client
    // borrow would otherwise lock out.
    let Some(client) = state.clients.get(client_id) else {
        return;
    };
    // Carried down for the same reason as a toplevel's: `repositioned` is
    // gated on version 3, and a popup left at version 1 would never get one.
    let version = client.version(xdg_surface_id);
    if client
        .register_with_version(popup_id, ObjectType::XdgPopup, version)
        .is_err()
    {
        return;
    }
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
            surface.subsurface_position = super::state::clamp_surface_offset(x, y);
        }
        if let Some(parent) = state.surfaces.get_mut(&(client_id, parent_wl)) {
            parent.children.push(popup_wl);
        }
    }

    // Send xdg_popup.configure with the computed position
    super::xdg_popup::send_configure(state, client_id, popup_id, x, y, width, height);

    // Send xdg_surface.configure with a serial the client must ack
    let serial = super::next_serial();
    let configure_args = ArgWriter::new().u32(serial).build();
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let _ = client.send(message(xdg_surface_id, CONFIGURE, configure_args));
}

fn handle_set_window_geometry(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y), Some(w), Some(h)) = (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        super::malformed_request(state, msg, "xdg_surface");
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
        super::malformed_request(state, msg, "xdg_surface");
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

/// Send an `xdg_surface.configure` event.
#[allow(dead_code)]
pub fn send_configure(
    state: &mut CompositorState,
    client_id: u32,
    xdg_surface_id: u32,
    serial: u32,
) {
    let args = ArgWriter::new().u32(serial).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(xdg_surface_id, CONFIGURE, args));
    }
}
