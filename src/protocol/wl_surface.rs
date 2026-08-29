//! `wl_surface` protocol handler.
//!
//! A surface is a rectangular area of pixels that can be displayed. Clients
//! attach buffers, mark damage, request frame callbacks, and commit to make
//! changes visible. The compositor reads committed state during rendering.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::state::{CompositorState, PendingInputRegion};
use super::wire_utils::{ArgReader, ArgWriter, message};

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

// Event opcodes
const ENTER: u16 = 0;
const LEAVE: u16 = 1;

/// Send `wl_surface.enter` — this surface is now shown on the given output.
///
/// Clients need this to choose a buffer scale: `wl_output.scale` is per output,
/// so a surface cannot know what scale to render at until it knows which
/// outputs it is on. `output_object_id` is the client's own `wl_output` object,
/// not the compositor's internal id.
pub fn send_enter(
    state: &mut CompositorState,
    client_id: u32,
    surface_id: u32,
    output_object_id: u32,
) {
    debug!("wl_surface.enter: surface_id={surface_id} output={output_object_id}");
    if let Some(client) = state.clients.get(client_id) {
        let args = ArgWriter::new().u32(output_object_id).build();
        let _ = client.send(message(surface_id, ENTER, args));
    }
}

/// Send `wl_surface.leave` — this surface is no longer shown on the given output.
pub fn send_leave(
    state: &mut CompositorState,
    client_id: u32,
    surface_id: u32,
    output_object_id: u32,
) {
    debug!("wl_surface.leave: surface_id={surface_id} output={output_object_id}");
    if let Some(client) = state.clients.get(client_id) {
        let args = ArgWriter::new().u32(output_object_id).build();
        let _ = client.send(message(surface_id, LEAVE, args));
    }
}

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        ATTACH => handle_attach(state, msg),
        DAMAGE | DAMAGE_BUFFER => handle_damage(state, msg),
        FRAME => handle_frame(state, msg),
        SET_INPUT_REGION => handle_set_input_region(state, msg),
        SET_BUFFER_SCALE => handle_set_buffer_scale(state, msg),
        SET_OPAQUE_REGION | SET_BUFFER_TRANSFORM | OFFSET => {
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
    state.destroy_surface(msg.client_id, surface_id);
    state.dirty = true;
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(surface_id);
    }
}

fn handle_attach(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // attach args: object buffer (id or 0 for null), int32 x, int32 y
    let (Some(buffer_id), Some(_x), Some(_y)) = (args.u32(), args.i32(), args.i32()) else {
        return;
    };

    let surface_id = msg.message.object_id;
    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        surface.pending.buffer_attached = true;
        // buffer_id 0 means detach
        surface.pending.buffer_id = if buffer_id == 0 {
            None
        } else {
            Some(buffer_id)
        };
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
    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        surface.pending.damage.push((x, y, w, h));
    }
}

fn handle_frame(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    // frame args: new_id callback
    let Some(callback_id) = args.new_id() else {
        return;
    };

    let surface_id = msg.message.object_id;

    if client
        .register(callback_id, ObjectType::WlCallback)
        .is_err()
    {
        return;
    }

    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        surface.pending.frame_callback = Some(callback_id);
    }
}

/// `wl_surface.set_input_region` — restrict which parts of the surface accept
/// pointer input.
///
/// The region is *copied* rather than referenced: the protocol lets the client
/// destroy the `wl_region` immediately afterwards, and later changes to that
/// object must not affect the surface. Like most surface state it is
/// double-buffered, so it only takes effect on the next commit.
fn handle_set_input_region(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let Some(region_id) = ArgReader::new(&msg.message.args).u32() else {
        return;
    };
    let surface_id = msg.message.object_id;

    // A null region is the protocol's way of saying "infinite": the whole
    // surface accepts input again.
    let region = if region_id == 0 {
        PendingInputRegion::Infinite
    } else if let Some(region) = state.regions.get(&(msg.client_id, region_id)) {
        PendingInputRegion::Rects(region.rects.clone())
    } else {
        tracing::warn!(
            "wl_surface.set_input_region: unknown region {} for surface {}",
            region_id,
            surface_id
        );
        return;
    };

    debug!(
        "wl_surface.set_input_region: surface_id={} region_id={}",
        surface_id, region_id
    );

    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        surface.pending.input_region = region;
    }
}

/// `wl_surface.set_buffer_scale` — how many buffer pixels map to one
/// surface-local coordinate.
///
/// A client on a scaled output submits a buffer this many times larger and sets
/// the scale, so the compositor knows the surface is still its logical size.
/// Double-buffered like the rest of surface state.
fn handle_set_buffer_scale(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let Some(scale) = ArgReader::new(&msg.message.args).i32() else {
        return;
    };
    let surface_id = msg.message.object_id;

    if scale < 1 {
        if let Some(client) = state.clients.get(msg.client_id) {
            // WL_SURFACE_ERROR_INVALID_SCALE = 0
            client.send_error(
                surface_id,
                0,
                "wl_surface.set_buffer_scale: scale must be >= 1",
            );
        }
        return;
    }

    debug!(
        "wl_surface.set_buffer_scale: surface_id={} scale={}",
        surface_id, scale
    );
    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        surface.pending.buffer_scale = Some(scale);
    }
}

fn handle_commit(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let surface_id = msg.message.object_id;
    let client_id = msg.client_id;
    let key = (client_id, surface_id);

    if let Some(surface) = state.surfaces.get_mut(&key) {
        // Apply pending buffer, releasing the old one if it changed
        if surface.pending.buffer_attached {
            let new_buffer = surface.pending.buffer_id.take();
            surface.pending.buffer_attached = false;
            if let Some(old_buffer) = surface.buffer_id
                && new_buffer != Some(old_buffer)
            {
                state.buffers_pending_release.push((client_id, old_buffer));
            }
            surface.buffer_id = new_buffer;
        }

        // Move frame callback to committed state (fired on next render)
        if surface.pending.frame_callback.is_some() {
            surface.frame_callback = surface.pending.frame_callback.take();
        }

        // Move presentation feedbacks to committed state
        surface
            .presentation_feedbacks
            .append(&mut surface.pending.presentation_feedbacks);

        // Apply pending buffer scale
        if let Some(scale) = surface.pending.buffer_scale.take() {
            surface.buffer_scale = scale;
        }

        // Apply pending input region
        match std::mem::take(&mut surface.pending.input_region) {
            PendingInputRegion::Unchanged => {}
            PendingInputRegion::Infinite => surface.input_region = None,
            PendingInputRegion::Rects(rects) => surface.input_region = Some(rects),
        }

        // Clear pending damage
        surface.pending.damage.clear();

        state.dirty = true;
        debug!(
            "wl_surface.commit: surface_id={} buffer={:?}",
            surface_id, surface.buffer_id
        );
    }

    // Apply pending viewport state
    if let Some(&vp_id) = state.surface_viewport.get(&(client_id, surface_id))
        && let Some(vp) = state.viewports.get_mut(&(client_id, vp_id))
    {
        if let Some(src) = vp.pending_source.take() {
            vp.source = Some(src);
        }
        if let Some(dst) = vp.pending_destination.take() {
            vp.destination = Some(dst);
        }
    }
}
