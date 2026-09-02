//! `wl_pointer` protocol handler.
//!
//! Delivers pointer (mouse) events to focused clients: enter, leave,
//! motion, button, and axis (scroll) events.

use crate::compositor::protocol::ArgReader;
use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::next_serial;
use super::state::CompositorState;
use super::wire_utils::{ArgWriter, message};

// Request opcodes
const SET_CURSOR: u16 = 0;
const RELEASE: u16 = 1;

// Event opcodes
pub const ENTER: u16 = 0;
pub const LEAVE: u16 = 1;
pub const MOTION: u16 = 2;
pub const BUTTON: u16 = 3;
pub const AXIS: u16 = 4;
pub const FRAME: u16 = 5;

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        SET_CURSOR => {
            process_set_cursor(state, msg);
        }
        RELEASE => {
            let pointer_id = msg.message.object_id;
            state
                .pointers
                .retain(|p| !(p.client_id == msg.client_id && p.object_id == pointer_id));
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(pointer_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        _ => super::unknown_request(state, msg, "wl_pointer"),
    }
}

fn process_set_cursor(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(serial), Some(surface_id), Some(hotspot_x), Some(hotspot_y)) =
        (args.u32(), args.u32(), args.i32(), args.i32())
    else {
        super::malformed_request(state, msg, "wl_pointer");
        return;
    };

    let client_id = msg.client_id;

    // Validate serial matches the most recent enter serial for this client.
    if state.pointer_enter_serial.get(&client_id) != Some(&serial) {
        return;
    }

    // surface_id == 0 means hide cursor.
    if surface_id == 0 {
        state.cursor_surfaces.insert(client_id, None);
        state.dirty = true;
        return;
    }

    let surface_key = (client_id, surface_id);

    // Check the surface exists.
    if !state.surfaces.contains_key(&surface_key) {
        return;
    }

    // Role check: surface must not already have another role (subsurface or xdg_surface).
    let is_subsurface = state
        .surfaces
        .get(&surface_key)
        .is_some_and(|s| s.parent.is_some());
    let is_xdg = state
        .xdg_surfaces
        .values()
        .any(|x| x.client_id == client_id && x.wl_surface_id == surface_id);

    if is_subsurface || is_xdg {
        return;
    }

    // Assign cursor role (permanent).
    state.cursor_role_surfaces.insert(surface_key);

    state
        .cursor_surfaces
        .insert(client_id, Some((surface_id, hotspot_x, hotspot_y)));
    state.dirty = true;
}

/// Send `wl_pointer.enter` to a client's pointer object.
pub fn send_enter(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    surface_id: u32,
    x: f64,
    y: f64,
) {
    let serial = next_serial();
    state.pointer_enter_serial.insert(client_id, serial);
    let args = ArgWriter::new()
        .u32(serial)
        .u32(surface_id)
        .fixed(x)
        .fixed(y)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(pointer_id, ENTER, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send `wl_pointer.leave` to a client's pointer object.
pub fn send_leave(state: &mut CompositorState, client_id: u32, pointer_id: u32, surface_id: u32) {
    let serial = next_serial();
    let args = ArgWriter::new().u32(serial).u32(surface_id).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(pointer_id, LEAVE, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send `wl_pointer.motion` to a client's pointer object.
pub fn send_motion(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    time_ms: u32,
    x: f64,
    y: f64,
) {
    let args = ArgWriter::new().u32(time_ms).fixed(x).fixed(y).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(pointer_id, MOTION, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send `wl_pointer.button` to a client's pointer object.
///
/// Returns the serial, which the compositor records so a client can quote it
/// when asking to start an interactive move or resize.
pub fn send_button(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    time_ms: u32,
    button: u32,
    pressed: bool,
) -> u32 {
    let serial = next_serial();
    let btn_state: u32 = u32::from(pressed); // 1 for pressed, 0 for released
    let args = ArgWriter::new()
        .u32(serial)
        .u32(time_ms)
        .u32(button)
        .u32(btn_state)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(pointer_id, BUTTON, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
    serial
}

/// Send `wl_pointer.axis` to a client's pointer object.
pub fn send_axis(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    time_ms: u32,
    axis: u32,
    value: f64,
) {
    let args = ArgWriter::new().u32(time_ms).u32(axis).fixed(value).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(pointer_id, AXIS, args));
    }
}

/// Send `wl_pointer.frame` to indicate end of a group of events (version 5+).
pub fn send_frame(state: &mut CompositorState, client_id: u32, pointer_id: u32) {
    if let Some(client) = state.clients.get(client_id)
        && client.version(pointer_id) >= 5
    {
        let _ = client.send(message(pointer_id, FRAME, Vec::new()));
    }
}
