//! wl_pointer protocol handler.
//!
//! Delivers pointer (mouse) events to focused clients: enter, leave,
//! motion, button, and axis (scroll) events.

use crate::protocol::ArgReader;
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

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
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
                client.unregister(pointer_id).await;
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        op => {
            tracing::warn!("wl_pointer: unhandled opcode {}", op);
        }
    }
}

fn process_set_cursor(_state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    if let (Some(_serial), Some(_surface), Some(_hotspot_x), Some(_hotspot_y)) =
        (args.u32(), args.u32(), args.i32(), args.i32())
    {
        // TODO: Process set cursor message
    }
}

/// Send wl_pointer.enter to a client's pointer object.
pub async fn send_enter(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    surface_id: u32,
    x: f64,
    y: f64,
) {
    let serial = next_serial();
    let args = ArgWriter::new()
        .u32(serial)
        .u32(surface_id)
        .fixed(x)
        .fixed(y)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(pointer_id, ENTER, args)).await;
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send wl_pointer.leave to a client's pointer object.
pub async fn send_leave(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    surface_id: u32,
) {
    let serial = next_serial();
    let args = ArgWriter::new().u32(serial).u32(surface_id).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(pointer_id, LEAVE, args)).await;
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send wl_pointer.motion to a client's pointer object.
pub async fn send_motion(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    time_ms: u32,
    x: f64,
    y: f64,
) {
    let args = ArgWriter::new().u32(time_ms).fixed(x).fixed(y).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(pointer_id, MOTION, args)).await;
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send wl_pointer.button to a client's pointer object.
pub async fn send_button(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    time_ms: u32,
    button: u32,
    pressed: bool,
) {
    let serial = next_serial();
    let btn_state: u32 = if pressed { 1 } else { 0 };
    let args = ArgWriter::new()
        .u32(serial)
        .u32(time_ms)
        .u32(button)
        .u32(btn_state)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(pointer_id, BUTTON, args)).await;
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send wl_pointer.axis to a client's pointer object.
pub async fn send_axis(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    time_ms: u32,
    axis: u32,
    value: f64,
) {
    let args = ArgWriter::new().u32(time_ms).u32(axis).fixed(value).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(pointer_id, AXIS, args)).await;
    }
}

/// Send wl_pointer.frame to indicate end of a group of events (version 5+).
pub async fn send_frame(state: &mut CompositorState, client_id: u32, pointer_id: u32) {
    if let Some(client) = state.clients.get(client_id)
        && client.version(pointer_id) >= 5
    {
        let _ = client.send(message(pointer_id, FRAME, Vec::new())).await;
    }
}
