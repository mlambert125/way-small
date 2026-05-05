//! wl_surface protocol handler.
//!
//! A surface is a rectangular area of pixels that can be displayed. Clients
//! attach buffers, mark damage, request frame callbacks, and commit to make
//! changes visible. The compositor reads committed state during rendering.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire::ArgReader;
use super::ObjectType;

// Request opcodes
const DESTROY: u16 = 0;
const ATTACH: u16 = 1;
const DAMAGE: u16 = 2;
const FRAME: u16 = 3;
const SET_OPAQUE_REGION: u16 = 4;
const SET_INPUT_REGION: u16 = 5;
const COMMIT: u16 = 6;
const SET_BUFFER_TRANSFORM: u16 = 7;
const SET_BUFFER_SCALE: u16 = 8;
const DAMAGE_BUFFER: u16 = 9;
const OFFSET: u16 = 10;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        ATTACH => handle_attach(state, msg),
        DAMAGE | DAMAGE_BUFFER => handle_damage(state, msg),
        FRAME => handle_frame(state, msg),
        SET_OPAQUE_REGION | SET_INPUT_REGION => {
            // Ignored until wl_region is fully wired to clipping
        }
        SET_BUFFER_TRANSFORM | SET_BUFFER_SCALE | OFFSET => {
            // Acknowledged but not acted upon yet
        }
        COMMIT => handle_commit(state, msg),
        op => {
            tracing::warn!("wl_surface: unhandled opcode {}", op);
        }
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let surface_id = msg.message.object_id;
    debug!("wl_surface.destroy: surface_id={}", surface_id);
    state.destroy_surface(surface_id);
    let client = state.clients.get_or_create(msg.client_id);
    client.unregister(surface_id);
}

fn handle_attach(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // attach args: object buffer (id or 0 for null), int32 x, int32 y
    let (Some(buffer_id), Some(_x), Some(_y)) = (args.u32(), args.i32(), args.i32()) else {
        return;
    };

    let surface_id = msg.message.object_id;
    if let Some(surface) = state.surfaces.get_mut(&surface_id) {
        // buffer_id 0 means detach
        surface.pending.buffer_id = if buffer_id == 0 { None } else { Some(buffer_id) };
    }
}

fn handle_damage(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // damage args: int32 x, int32 y, int32 width, int32 height
    let (Some(x), Some(y), Some(w), Some(h)) = (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        return;
    };

    let surface_id = msg.message.object_id;
    if let Some(surface) = state.surfaces.get_mut(&surface_id) {
        surface.pending.damage.push((x, y, w, h));
    }
}

fn handle_frame(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // frame args: new_id callback
    let Some(callback_id) = args.new_id() else {
        return;
    };

    let surface_id = msg.message.object_id;

    let client = state.clients.get_or_create(msg.client_id);
    client.register(callback_id, ObjectType::WlCallback);

    if let Some(surface) = state.surfaces.get_mut(&surface_id) {
        surface.pending.frame_callback = Some(callback_id);
    }
}

fn handle_commit(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let surface_id = msg.message.object_id;

    if let Some(surface) = state.surfaces.get_mut(&surface_id) {
        // Apply pending buffer
        if surface.pending.buffer_id.is_some() {
            surface.buffer_id = surface.pending.buffer_id.take();
        }

        // Move frame callback to committed state (fired on next render)
        if surface.pending.frame_callback.is_some() {
            surface.frame_callback = surface.pending.frame_callback.take();
        }

        // Clear pending damage
        surface.pending.damage.clear();

        debug!(
            "wl_surface.commit: surface_id={} buffer={:?}",
            surface_id, surface.buffer_id
        );
    }
}
