//! `wl_surface` protocol handler.
//!
//! A surface is a rectangular area of pixels that can be displayed. Clients
//! attach buffers, mark damage, request frame callbacks, and commit to make
//! changes visible. The compositor reads committed state during rendering.

use tracing::debug;

use crate::shared::TextureRect;
use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::state::{ClientObjectId, CompositorState, PendingInputRegion};
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
        // The two damage requests differ in coordinate space, so they cannot
        // share a list: `damage` is surface-local, `damage_buffer` is in
        // buffer pixels, and only the latter is directly usable as an upload
        // region.
        DAMAGE => handle_damage(state, msg, DamageSpace::Surface),
        DAMAGE_BUFFER => handle_damage(state, msg, DamageSpace::Buffer),
        FRAME => handle_frame(state, msg),
        SET_INPUT_REGION => handle_set_input_region(state, msg),
        SET_BUFFER_SCALE => handle_set_buffer_scale(state, msg),
        OFFSET => handle_offset(state, msg),
        SET_OPAQUE_REGION | SET_BUFFER_TRANSFORM => {
            // Acknowledged but not acted upon yet
        }
        COMMIT => handle_commit(state, msg),
        _ => super::unknown_request(state, msg, "wl_surface"),
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
    let (Some(buffer_id), Some(x), Some(y)) = (args.u32(), args.i32(), args.i32()) else {
        super::malformed_request(state, msg, "wl_surface");
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
        accumulate_offset(&mut surface.pending.offset, x, y);
    }
}

fn handle_offset(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y)) = (args.i32(), args.i32()) else {
        super::malformed_request(state, msg, "wl_surface");
        return;
    };

    let surface_id = msg.message.object_id;
    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        accumulate_offset(&mut surface.pending.offset, x, y);
    }
}

/// Add one offset onto the pending one, keeping it within bounds.
///
/// Offsets accumulate across attaches within a commit, so this adds rather than
/// replaces. Clamped for the same reason a subsurface position is: these are
/// raw `i32`s from the client and are later added to a cursor position, which
/// overflows if nothing bounds them.
fn accumulate_offset(pending: &mut (i32, i32), x: i32, y: i32) {
    *pending = super::state::clamp_surface_offset(
        pending.0.saturating_add(x),
        pending.1.saturating_add(y),
    );
}

/// Which coordinate space a damage rectangle arrived in.
#[derive(Debug, Clone, Copy)]
enum DamageSpace {
    /// `wl_surface.damage` — surface-local, so it has to be mapped through the
    /// viewport and buffer scale before it means anything to a texture.
    Surface,
    /// `wl_surface.damage_buffer` — already buffer pixels.
    Buffer,
}

fn handle_damage(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
    space: DamageSpace,
) {
    let mut args = ArgReader::new(&msg.message.args);
    // damage args: int32 x, int32 y, int32 width, int32 height
    let (Some(x), Some(y), Some(width), Some(height)) =
        (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        super::malformed_request(state, msg, "wl_surface");
        return;
    };

    let surface_id = msg.message.object_id;
    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        let rect = TextureRect {
            x,
            y,
            width,
            height,
        };
        match space {
            DamageSpace::Surface => surface.pending.damage_surface.push(rect),
            DamageSpace::Buffer => surface.pending.damage_buffer.push(rect),
        }
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
        super::malformed_request(state, msg, "wl_surface");
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
    let mut committed_buffer = None;
    let mut damage_surface = Vec::new();
    let mut damage_buffer = Vec::new();

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

        // Apply the accumulated attach offset. Only the drag icon reads it —
        // see `Surface::offset` — but it is committed here like any other
        // double-buffered state so that it is correct if anything else comes
        // to need it.
        let offset = std::mem::take(&mut surface.pending.offset);
        surface.offset = super::state::clamp_surface_offset(
            surface.offset.0.saturating_add(offset.0),
            surface.offset.1.saturating_add(offset.1),
        );

        // Apply pending input region
        match std::mem::take(&mut surface.pending.input_region) {
            PendingInputRegion::Unchanged => {}
            PendingInputRegion::Infinite => surface.input_region = None,
            PendingInputRegion::Rects(rects) => surface.input_region = Some(rects),
        }

        damage_surface = std::mem::take(&mut surface.pending.damage_surface);
        damage_buffer = std::mem::take(&mut surface.pending.damage_buffer);

        committed_buffer = surface.buffer_id;

        state.dirty = true;
        debug!(
            "wl_surface.commit: surface_id={} buffer={:?}",
            surface_id, committed_buffer
        );
    }

    // Apply pending viewport state before reading damage: surface-local damage
    // is interpreted through the viewport this same commit installs.
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

    // A commit is the only point at which a client promises its buffer contents
    // are stable, and it may have redrawn into a buffer it never detached, so
    // every commit marks the attached buffer changed. What the damage adds is
    // how *much* of it changed.
    if let Some(buffer_id) = committed_buffer {
        let damage = committed_damage(state, key, buffer_id, &damage_surface, &damage_buffer);
        state.mark_buffer_damaged(client_id, buffer_id, &damage);
    }
}

/// Convert a commit's damage into buffer-pixel rectangles.
///
/// An empty result means "assume the whole buffer changed", and every
/// uncertain case collapses to it: damage is a promise that everything
/// *outside* it is unchanged, so a rectangle we cannot place accurately is
/// worse than no rectangle at all.
fn committed_damage(
    state: &CompositorState,
    key: ClientObjectId,
    buffer_id: u32,
    damage_surface: &[TextureRect],
    damage_buffer: &[TextureRect],
) -> Vec<TextureRect> {
    if damage_surface.is_empty() && damage_buffer.is_empty() {
        return Vec::new();
    }
    let Some(buffer) = state.buffers.get(&(key.0, buffer_id)) else {
        return Vec::new();
    };
    let (width, height) = (buffer.width, buffer.height);

    let mut rects = Vec::with_capacity(damage_surface.len() + damage_buffer.len());
    rects.extend(
        damage_buffer
            .iter()
            .filter_map(|r| clamp_rect(*r, width, height)),
    );

    if !damage_surface.is_empty() {
        // Surface coordinates only mean something through the same mapping the
        // scene draws with, run backwards. Without it, nothing can be placed.
        let Some(mapping) = state.surface_buffer_mapping(key) else {
            return Vec::new();
        };
        let (src_x, src_y, src_w, src_h) = mapping.src;
        let scale_x = src_w / f64::from(mapping.dest_width);
        let scale_y = src_h / f64::from(mapping.dest_height);

        for rect in damage_surface {
            // Round outward and pad by a pixel. The mapping is not pixel-exact,
            // and uploading a little more than changed is always safe.
            let x0 = (src_x + f64::from(rect.x) * scale_x).floor() - 1.0;
            let y0 = (src_y + f64::from(rect.y) * scale_y).floor() - 1.0;
            let x1 = (src_x + f64::from(rect.x.saturating_add(rect.width)) * scale_x).ceil() + 1.0;
            let y1 = (src_y + f64::from(rect.y.saturating_add(rect.height)) * scale_y).ceil() + 1.0;
            let mapped = TextureRect {
                x: clamped_i32(x0),
                y: clamped_i32(y0),
                width: clamped_i32(x1 - x0),
                height: clamped_i32(y1 - y0),
            };
            if let Some(clamped) = clamp_rect(mapped, width, height) {
                rects.push(clamped);
            }
        }
    }

    rects
}

/// Clip a rectangle to a buffer, dropping it if nothing is left.
fn clamp_rect(rect: TextureRect, width: i32, height: i32) -> Option<TextureRect> {
    let x0 = rect.x.max(0);
    let y0 = rect.y.max(0);
    let x1 = rect.x.saturating_add(rect.width).min(width);
    let y1 = rect.y.saturating_add(rect.height).min(height);
    (x1 > x0 && y1 > y0).then_some(TextureRect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

/// Narrow a mapped coordinate to `i32`, saturating rather than wrapping.
///
/// These values come from client-supplied rectangles scaled by a client-chosen
/// viewport, so they can be any float at all; `as` saturates, which is what we
/// want, but the intent is worth naming.
#[allow(clippy::cast_possible_truncation)]
fn clamped_i32(value: f64) -> i32 {
    value as i32
}

#[cfg(test)]
mod tests;
