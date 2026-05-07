//! xdg_toplevel protocol handler.
//!
//! A toplevel is the primary window role. The compositor sends configure
//! events (size, states like maximized/fullscreen), and the client can
//! set title, app_id, and request state changes.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire::{ArgReader, ArgWriter, message};

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
#[allow(dead_code)]
const CLOSE: u16 = 1;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        SET_TITLE => handle_set_title(state, msg),
        SET_APP_ID => handle_set_app_id(state, msg),
        SET_PARENT | SHOW_WINDOW_MENU | MOVE | RESIZE => {
            debug!("xdg_toplevel: interactive op {} (not yet implemented)", msg.message.op_code);
        }
        SET_MAX_SIZE | SET_MIN_SIZE => {
            // Acknowledge but don't enforce yet
        }
        SET_MAXIMIZED | UNSET_MAXIMIZED | SET_FULLSCREEN | UNSET_FULLSCREEN | SET_MINIMIZED => {
            debug!("xdg_toplevel: state change op {} (not yet implemented)", msg.message.op_code);
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
    let client = state.clients.get_or_create(msg.client_id);
    client.unregister(toplevel_id);
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

/// Send xdg_toplevel.configure event (width, height, states array).
pub async fn send_configure(
    state: &mut CompositorState,
    client_id: u32,
    toplevel_id: u32,
    width: i32,
    height: i32,
) {
    // states is an array of u32 enum values (empty = no special state)
    let states: Vec<u8> = Vec::new();
    let args = ArgWriter::new()
        .i32(width)
        .i32(height)
        .u32(states.len() as u32) // wl_array length
        .build();
    let client = state.clients.get_or_create(client_id);
    let _ = client.send(message(toplevel_id, CONFIGURE, args)).await;
}

/// Send xdg_toplevel.close event to request the client closes.
#[allow(dead_code)]
pub async fn send_close(state: &mut CompositorState, client_id: u32, toplevel_id: u32) {
    let client = state.clients.get_or_create(client_id);
    let _ = client.send(message(toplevel_id, CLOSE, Vec::new())).await;
}
