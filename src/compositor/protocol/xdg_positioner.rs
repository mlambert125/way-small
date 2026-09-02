//! `xdg_positioner` protocol handler.
//!
//! A positioner describes how a popup surface should be placed relative to
//! its parent. Clients set size, anchor rect, anchor edge, gravity, offset,
//! and constraint adjustments before passing the positioner to `get_popup`.

use enumflags2::BitFlags;

use super::state::{XdgPositionerAnchor, XdgPositionerGravity};
use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire_utils::ArgReader;

// Request opcodes
const DESTROY: u16 = 0;
const SET_SIZE: u16 = 1;
const SET_ANCHOR_RECT: u16 = 2;
const SET_ANCHOR: u16 = 3;
const SET_GRAVITY: u16 = 4;
const SET_CONSTRAINT_ADJUSTMENT: u16 = 5;
const SET_OFFSET: u16 = 6;
// Since version 3. Advertised — xdg_wm_base goes out at version 5 — so they
// must be in the match even though nothing acts on them yet; an opcode a
// client is entitled to send has to be told apart from one that does not
// exist. Popups here are positioned once and not repositioned, so a reactive
// popup simply stays where it was put.
const SET_REACTIVE: u16 = 7;
const SET_PARENT_SIZE: u16 = 8;
const SET_PARENT_CONFIGURE: u16 = 9;

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        SET_SIZE => handle_set_size(state, msg),
        SET_ANCHOR_RECT => handle_set_anchor_rect(state, msg),
        SET_ANCHOR => handle_set_anchor(state, msg),
        SET_GRAVITY => handle_set_gravity(state, msg),
        SET_CONSTRAINT_ADJUSTMENT => handle_set_constraint_adjustment(state, msg),
        SET_OFFSET => handle_set_offset(state, msg),
        SET_REACTIVE | SET_PARENT_SIZE | SET_PARENT_CONFIGURE => {
            // Accepted but not acted upon yet
        }
        _ => super::unknown_request(state, msg, "xdg_positioner"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let id = msg.message.object_id;
    state.destroy_xdg_positioner(msg.client_id, id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

fn handle_set_size(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(width), Some(height)) = (args.i32(), args.i32()) else {
        super::malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.width = width;
        p.height = height;
    }
}

fn handle_set_anchor_rect(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y), Some(w), Some(h)) = (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        super::malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.anchor_rect = (x, y, w, h);
    }
}

fn handle_set_anchor(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(anchor) = args.u32() else {
        super::malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.anchor = XdgPositionerAnchor::from_repr(anchor).expect("Anchor value should be valid");
    }
}

fn handle_set_gravity(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(gravity) = args.u32() else {
        super::malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.gravity =
            XdgPositionerGravity::from_repr(gravity).expect("Gravity value should be valid");
    }
}

fn handle_set_constraint_adjustment(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(adjustment) = args.u32() else {
        super::malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.constraint_adjustment = BitFlags::from_bits_truncate(adjustment);
    }
}

fn handle_set_offset(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y)) = (args.i32(), args.i32()) else {
        super::malformed_request(state, msg, "xdg_positioner");
        return;
    };
    let id = msg.message.object_id;
    if let Some(p) = state.xdg_positioners.get_mut(&(msg.client_id, id)) {
        p.offset = (x, y);
    }
}
