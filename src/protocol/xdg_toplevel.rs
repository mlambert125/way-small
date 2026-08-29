//! `xdg_toplevel` protocol handler.
//!
//! A toplevel is the primary window role. The compositor sends configure
//! events (size, states like maximized/fullscreen), and the client can
//! set title, `app_id`, and request state changes.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire_utils::{ArgReader, ArgWriter, message};

// Request opcodes
const DESTROY: u16 = 0;
const SET_PARENT: u16 = 1;
const SET_TITLE: u16 = 2;
const SET_APP_ID: u16 = 3;
const SHOW_WINDOW_MENU: u16 = 4;
const MOVE: u16 = 5;
const RESIZE: u16 = 6;
const SET_MAX_SIZE: u16 = 7;
const SET_MIN_SIZE: u16 = 8;
const SET_MAXIMIZED: u16 = 9;
const UNSET_MAXIMIZED: u16 = 10;
const SET_FULLSCREEN: u16 = 11;
const UNSET_FULLSCREEN: u16 = 12;
const SET_MINIMIZED: u16 = 13;

// Event opcodes
const CONFIGURE: u16 = 0;
const CLOSE: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        SET_TITLE => handle_set_title(state, msg),
        SET_APP_ID => handle_set_app_id(state, msg),
        SET_PARENT | SHOW_WINDOW_MENU | MOVE | RESIZE => {
            debug!(
                "xdg_toplevel: interactive op {} (not yet implemented)",
                msg.message.op_code
            );
        }
        SET_MAX_SIZE | SET_MIN_SIZE => {
            // Acknowledge but don't enforce yet
        }
        SET_MAXIMIZED | UNSET_MAXIMIZED | SET_FULLSCREEN | UNSET_FULLSCREEN | SET_MINIMIZED => {
            debug!(
                "xdg_toplevel: state change op {} (not yet implemented)",
                msg.message.op_code
            );
        }
        op => {
            tracing::warn!("xdg_toplevel: unhandled opcode {}", op);
        }
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let toplevel_id = msg.message.object_id;
    debug!("xdg_toplevel.destroy: toplevel_id={}", toplevel_id);
    state.destroy_xdg_toplevel(msg.client_id, toplevel_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(toplevel_id);
    }
}

fn handle_set_title(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(title) = args.string() else {
        return;
    };
    let toplevel_id = msg.message.object_id;
    debug!("xdg_toplevel.set_title: \"{}\"", title);
    if let Some(toplevel) = state.xdg_toplevels.get_mut(&(msg.client_id, toplevel_id)) {
        toplevel.title = Some(title);
    }
}

fn handle_set_app_id(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(app_id) = args.string() else {
        return;
    };
    let toplevel_id = msg.message.object_id;
    debug!("xdg_toplevel.set_app_id: \"{}\"", app_id);
    if let Some(toplevel) = state.xdg_toplevels.get_mut(&(msg.client_id, toplevel_id)) {
        toplevel.app_id = Some(app_id);
    }
}

// `xdg_toplevel` state values
const STATE_ACTIVATED: u32 = 4;

/// Send `xdg_toplevel`.configure event (width, height, states array).
pub fn send_configure(
    state: &mut CompositorState,
    client_id: u32,
    toplevel_id: u32,
    width: i32,
    height: i32,
) {
    send_configure_with_states(state, client_id, toplevel_id, width, height, &[]);
}

/// Send `xdg_toplevel`.configure with explicit states.
fn send_configure_with_states(
    state: &mut CompositorState,
    client_id: u32,
    toplevel_id: u32,
    width: i32,
    height: i32,
    states: &[u32],
) {
    // Build args: i32 width, i32 height, wl_array(states)
    // wl_array = u32 byte-length + raw u32 values (already 4-byte aligned)
    let mut args = ArgWriter::new()
        .i32(width)
        .i32(height)
        .u32(u32::try_from(states.len() * 4).expect("States length should be < u32::MAX"));
    for &s in states {
        args = args.u32(s);
    }
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(toplevel_id, CONFIGURE, args.build()));
    }
}

/// Find the `xdg_surface` and `xdg_toplevel` object ids backing a client's
/// `wl_surface`, if that surface has the toplevel role.
///
/// Most compositor-side state is keyed by `wl_surface`, but the `xdg_shell`
/// events are addressed to the toplevel, so this bridges the two.
pub fn xdg_ids_for_surface(
    state: &CompositorState,
    client_id: u32,
    wl_surface_id: u32,
) -> Option<(u32, u32)> {
    state.xdg_surfaces.iter().find_map(|(key, xdg_surface)| {
        if key.0 != client_id || xdg_surface.wl_surface_id != wl_surface_id {
            return None;
        }
        match &xdg_surface.role {
            Some(super::state::XdgRole::Toplevel(tid)) => Some((key.1, *tid)),
            _ => None,
        }
    })
}

/// Send a configure sequence to set or clear the activated state on a toplevel.
/// Looks up the toplevel from a `wl_surface` id and sends toplevel.configure +
/// `xdg_surface.configure(serial)`.
pub fn send_activated(
    state: &mut CompositorState,
    client_id: u32,
    wl_surface_id: u32,
    activated: bool,
) {
    let client = state.clients.get(client_id);
    if client.is_none() {
        tracing::warn!("Received message from unknown client {}", client_id);
        return;
    }
    let Some((xdg_surface_id, toplevel_id)) = xdg_ids_for_surface(state, client_id, wl_surface_id)
    else {
        return;
    };

    let states: &[u32] = if activated { &[STATE_ACTIVATED] } else { &[] };
    send_configure_with_states(state, client_id, toplevel_id, 0, 0, states);

    // Send xdg_surface.configure with a serial
    let serial = super::next_serial();
    let args = ArgWriter::new().u32(serial).build();

    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(xdg_surface_id, super::xdg_surface::CONFIGURE, args));
    }
}

/// Send `xdg_toplevel.close`, asking the client to close the window.
///
/// This is a request, not a command: the client decides what to do, and may
/// prompt the user or ignore it entirely. A client that agrees destroys the
/// toplevel, which tears the window down through the normal destroy path.
pub fn send_close(state: &mut CompositorState, client_id: u32, toplevel_id: u32) {
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(toplevel_id, CLOSE, Vec::new()));
    }
}
