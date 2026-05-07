//! wl_region protocol handler.
//!
//! A region is a set of rectangles used to describe input and opaque areas
//! of a surface. Clients add/subtract rectangles, then assign the region
//! to a surface via set_opaque_region or set_input_region.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire::ArgReader;

// Request opcodes
const DESTROY: u16 = 0;
const ADD: u16 = 1;
const SUBTRACT: u16 = 2;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        ADD => handle_add(state, msg),
        SUBTRACT => handle_subtract(state, msg),
        op => {
            tracing::warn!("wl_region: unhandled opcode {}", op);
        }
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let region_id = msg.message.object_id;
    debug!("wl_region.destroy: region_id={}", region_id);
    state.destroy_region(msg.client_id, region_id);
    let client = state.clients.get_or_create(msg.client_id);
    client.unregister(region_id);
}

fn handle_add(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y), Some(w), Some(h)) = (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        return;
    };
    let region_id = msg.message.object_id;
    if let Some(region) = state.regions.get_mut(&(msg.client_id, region_id)) {
        region.rects.push((x, y, w, h));
    }
}

fn handle_subtract(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y), Some(w), Some(h)) = (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        return;
    };
    let region_id = msg.message.object_id;
    if let Some(region) = state.regions.get_mut(&(msg.client_id, region_id)) {
        region.subtracts.push((x, y, w, h));
    }
}
