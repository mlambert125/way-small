//! xdg_popup protocol handler.
//!
//! A popup is a short-lived surface (menu, tooltip, combo-box) positioned
//! relative to a parent xdg_surface using an xdg_positioner. The compositor
//! sends configure events with the computed position and size.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire_utils::{ArgWriter, message};

// Request opcodes
const DESTROY: u16 = 0;
const GRAB: u16 = 1;
#[allow(dead_code)]
const REPOSITION: u16 = 2;

// Event opcodes
const CONFIGURE: u16 = 0;
const POPUP_DONE: u16 = 1;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg).await,
        GRAB => handle_grab(msg),
        REPOSITION => {
            debug!("xdg_popup.reposition: not yet implemented");
        }
        op => {
            tracing::warn!("xdg_popup: unhandled opcode {}", op);
        }
    }
}

async fn handle_destroy(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let popup_id = msg.message.object_id;
    let client_id = msg.client_id;
    debug!("xdg_popup.destroy: popup_id={}", popup_id);

    // Remove the popup's wl_surface from its parent's children list
    if let Some(popup) = state.xdg_popups.get(&(client_id, popup_id)) {
        let xdg_surface_id = popup.xdg_surface_id;
        if let Some(xdg_surface) = state.xdg_surfaces.get(&(client_id, xdg_surface_id)) {
            let wl_surface_id = xdg_surface.wl_surface_id;
            if let Some(surface) = state.surfaces.get(&(client_id, wl_surface_id)) {
                let parent_id = surface.parent;
                if let Some(parent_id) = parent_id
                    && let Some(parent) = state.surfaces.get_mut(&(client_id, parent_id))
                {
                    parent.children.retain(|&id| id != wl_surface_id);
                }
            }
            // Clear the parent link
            if let Some(surface) = state.surfaces.get_mut(&(client_id, wl_surface_id)) {
                surface.parent = None;
            }
        }
    }

    state.destroy_xdg_popup(client_id, popup_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(popup_id).await;
    }
}

fn handle_grab(msg: &WaylandProtocolMessageWithClientInfo) {
    // grab args: object seat, uint serial
    // Acknowledged but not enforced — we don't do exclusive grabs yet
    debug!(
        "xdg_popup.grab: popup_id={} (acknowledged)",
        msg.message.object_id
    );
}

/// Send xdg_popup.configure event with the computed position and size.
pub async fn send_configure(
    state: &mut CompositorState,
    client_id: u32,
    popup_id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) {
    let args = ArgWriter::new()
        .i32(x)
        .i32(y)
        .i32(width)
        .i32(height)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(popup_id, CONFIGURE, args)).await;
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send xdg_popup.popup_done to dismiss the popup.
#[allow(dead_code)]
pub async fn send_popup_done(state: &mut CompositorState, client_id: u32, popup_id: u32) {
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(popup_id, POPUP_DONE, Vec::new())).await;
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}
