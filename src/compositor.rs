//! Compositor event loop.
//!
//! Receives messages from two sources: Wayland clients (protocol requests)
//! and the display backend (input events, resize, focus). Composites surfaces
//! into frames and sends them to the backend for display on a 60fps timer.

use std::time::{Duration, Instant};

use std::sync::Arc;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::backend::{BackendMessage, KeyState, MouseButton, RenderFrame};
use crate::protocol::state::ClientObjectId;
use crate::protocol::wire_utils::{ArgWriter, message};
use crate::protocol::{self, CompositorState};
use crate::protocol::{wl_keyboard, wl_pointer, wl_registry, xdg_popup, xdg_toplevel};
use crate::renderer;
use crate::wayland_socket::WaylandSocketMessage;

const FRAME_INTERVAL: Duration = Duration::from_millis(16); // ~60fps

#[allow(clippy::too_many_lines)]
pub async fn run_compositor(
    mut wayland_message_receiver: Receiver<WaylandSocketMessage>,
    mut backend_message_receiver: Receiver<BackendMessage>,
    frame_sender: Sender<Arc<RenderFrame>>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    info!("Running compositor...");

    let mut state = protocol::CompositorState::new();
    state.default_cursor = renderer::load_default_cursor();
    if state.default_cursor.is_none() {
        info!("No cursor theme found, using built-in cursor");
    }
    let mut render_timer = tokio::time::interval(FRAME_INTERVAL);
    let start_time = Instant::now();

    loop {
        tokio::select! {
            Some(message) = wayland_message_receiver.recv() => {
                // Process this message, then drain all queued messages before
                // returning to select. This avoids rendering intermediate states
                // during bursts of protocol traffic (e.g. client startup).
                let mut pending = vec![message];
                while let Ok(msg) = wayland_message_receiver.try_recv() {
                    pending.push(msg);
                }
                for message in pending {
                    match message {
                        WaylandSocketMessage::NewClient(msg)  => {
                            info!("New client connected: {}", msg.client_id);
                            state.clients.create(msg.client_id, msg.socket_sender, msg.fd_queue);
                        }
                        WaylandSocketMessage::Message(msg) => {
                            debug!(
                                "client {}: object_id={} op_code={}",
                                msg.client_id, msg.message.object_id, msg.message.op_code
                            );
                            protocol::handle_message(&mut state, &msg).await;
                        }
                        WaylandSocketMessage::ClientDisconnected { client_id } => {
                            info!("Client {} disconnected", client_id);
                            let was_focused = state.focused_surface.map(|(cid, _)| cid) == Some(client_id);
                            state.remove_client_resources(client_id);
                            state.clients.remove(client_id);
                            state.dirty = true;

                            // Focus the next surface down the stack
                            if was_focused && let Some(&new_key) = state.surface_stack.last() {
                                switch_focus(&mut state, new_key).await;
                            }
                        }
                    }
                }
            }
            Some(message) = backend_message_receiver.recv() => {
                match message {
                    BackendMessage::SeatCapabilities { pointer, keyboard } => {
                        info!("Seat capabilities: pointer={} keyboard={}", pointer, keyboard);
                        state.seat.has_pointer = pointer;
                        state.seat.has_keyboard = keyboard;
                    }
                    BackendMessage::OutputInfo { outputs } => {
                        for new_output in outputs {
                            if state.outputs.iter().any(|o| o.id == new_output.id) {
                                // Update existing output (preserve global name mapping)
                                if let Some(existing) = state.outputs.iter_mut().find(|o| o.id == new_output.id) {
                                    existing.geometry = new_output.geometry;
                                    existing.modes = new_output.modes;
                                    existing.scale = new_output.scale;
                                    existing.description = new_output.description;
                                }
                            } else {
                                // New output — assign a global name and advertise
                                let global_name = state.next_global_number;
                                state.next_global_number += 1;
                                state.output_global_names.insert(new_output.id, global_name);
                                state.outputs.push(new_output);
                                wl_registry::broadcast_output_global(&mut state, global_name).await;
                            }
                        }
                    }
                    BackendMessage::Closed => {
                        info!("Backend requested shutdown");
                        cancel_token.cancel();
                        break;
                    }
                    BackendMessage::Resized(output_id, w, h) => {
                        info!("Backend resized to {}x{}", w, h);

                        if let Some(output) = state.outputs.iter_mut().find(|o| o.id == output_id) {
                            output.geometry.physical_width = w;
                            output.geometry.physical_height = h;

                            output.modes.iter_mut().for_each(|m| {
                                m.width = w;
                                m.height = h;
                            });
                        }

                        protocol::wl_output::broadcast_mode(&mut state).await;
                        state.dirty = true;
                    }
                    BackendMessage::KeyInput { keycode, state: key_state, mods_depressed, mods_latched, mods_locked, mods_group } => {
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        let pressed = matches!(key_state, KeyState::Pressed);
                        let evdev_key = keycode.saturating_sub(8);
                        if pressed {
                            state.pressed_keys.insert(evdev_key);
                        } else {
                            state.pressed_keys.remove(&evdev_key);
                        }
                        // Only send key events to the focused surface's client
                        let focused_client = state.focused_surface.map(|(cid, _)| cid);
                        for kb in state.keyboards.clone() {
                            if Some(kb.client_id) == focused_client {
                                wl_keyboard::send_key(&mut state, kb.client_id, kb.object_id, time_ms, evdev_key, pressed).await;
                                wl_keyboard::send_modifiers(&mut state, kb.client_id, kb.object_id, mods_depressed, mods_latched, mods_locked, mods_group).await;
                            }
                        }
                    }

                    BackendMessage::MouseMove { x, y } => {
                        state.cursor_x = x;
                        state.cursor_y = y;
                        state.dirty = true;
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);

                        // Auto-focus the top surface if nothing is focused yet
                        if state.focused_surface.is_none() &&
                           let Some(&top_key) = state.surface_stack.last() &&
                           state.surfaces.contains_key(&top_key) {
                            switch_focus(&mut state, top_key).await;
                        }

                        // Determine which specific surface the pointer is over
                        let hit = hit_test(&state, x, y);
                        let new_pointer_surface = hit.as_ref().map(|h| h.surface);
                        let old_pointer_surface = state.pointer_surface;

                        // Send pointer enter/leave when the surface under the cursor changes
                        if new_pointer_surface != old_pointer_surface {
                            if let Some(old_ps) = old_pointer_surface {
                                for ptr in state.pointers.clone() {
                                    if ptr.client_id == old_ps.0 {
                                        wl_pointer::send_leave(&mut state, ptr.client_id, ptr.object_id, old_ps.1).await;
                                        wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id).await;
                                    }
                                }
                            }
                            state.pointer_surface = new_pointer_surface;
                            if let Some(ref h) = hit {
                                let local_x = x - f64::from(h.surface_x);
                                let local_y = y - f64::from(h.surface_y);
                                for ptr in state.pointers.clone() {
                                    if ptr.client_id == h.surface.0 {
                                        wl_pointer::send_enter(&mut state, ptr.client_id, ptr.object_id, h.surface.1, local_x, local_y).await;
                                        wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id).await;
                                    }
                                }
                            }
                        }

                        // Send motion to the current pointer surface
                        if let Some(ref h) = hit {
                            let local_x = x - f64::from(h.surface_x);
                            let local_y = y - f64::from(h.surface_y);
                            for ptr in state.pointers.clone() {
                                if ptr.client_id == h.surface.0 {
                                    wl_pointer::send_motion(&mut state, ptr.client_id, ptr.object_id, time_ms, local_x, local_y).await;
                                    wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id).await;
                                }
                            }
                        }
                    }
                    BackendMessage::MouseButton { button, state: btn_state } => {
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        let pressed = matches!(btn_state, crate::backend::ButtonState::Pressed);
                        // Linux evdev button codes
                        let linux_button = match button {
                            MouseButton::Left => 0x110,
                            MouseButton::Right => 0x111,
                            MouseButton::Middle => 0x112,
                        };

                        // Dismiss grabbed popups if click lands outside them
                        if pressed && !state.grabbed_popups.is_empty() {
                            let dismissed = dismiss_popups_outside_click(&mut state).await;
                            if dismissed {
                                // Don't process the click further — it was consumed by dismissal
                                continue;
                            }
                        }

                        // On press, hit-test and raise/focus the clicked surface
                        let cx = state.cursor_x;
                        let cy = state.cursor_y;
                        if pressed && let Some(hit) = hit_test(&state, cx, cy) && state.focused_surface != Some(hit.toplevel) {
                            // Raise to top of stack
                            state.surface_stack.retain(|k| *k != hit.toplevel);
                            state.surface_stack.push(hit.toplevel);
                            state.dirty = true;

                            switch_focus(&mut state, hit.toplevel).await;

                            // Update pointer surface to the specific surface under cursor
                            if state.pointer_surface != Some(hit.surface) {
                                state.pointer_surface = Some(hit.surface);
                                let local_x = cx - f64::from(hit.surface_x);
                                let local_y = cy - f64::from(hit.surface_y);
                                for ptr in state.pointers.clone() {
                                    if ptr.client_id == hit.surface.0 {
                                        wl_pointer::send_enter(&mut state, ptr.client_id, ptr.object_id, hit.surface.1, local_x, local_y).await;
                                        wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id).await;
                                    }
                                }
                            }
                        }

                        // Send button event to the pointer surface
                        if let Some(ps) = state.pointer_surface {
                            for ptr in state.pointers.clone() {
                                if ptr.client_id == ps.0 {
                                    wl_pointer::send_button(&mut state, ptr.client_id, ptr.object_id, time_ms, linux_button, pressed).await;
                                    wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id).await;
                                }
                            }
                        }
                    }
                    BackendMessage::MouseScroll { dx, dy } => {
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        let pointer_client = state.pointer_surface.map(|(cid, _)| cid);
                        for ptr in state.pointers.clone() {
                            if Some(ptr.client_id) != pointer_client {
                                continue;
                            }
                            if dy != 0.0 {
                                // mouse axis 0 = vertical
                                wl_pointer::send_axis(&mut state, ptr.client_id, ptr.object_id, time_ms, 0, dy * 10.0).await;
                            }
                            if dx != 0.0 {
                                // mouse axis 1 = horizontal
                                wl_pointer::send_axis(&mut state, ptr.client_id, ptr.object_id, time_ms, 1, dx * 10.0).await;
                            }
                            wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id).await;
                        }
                    }
                    BackendMessage::FocusIn => {
                        debug!("Focus in");
                    }
                    BackendMessage::FocusOut => {
                        debug!("Focus out");
                    }
                }
            }
            _ = render_timer.tick() => {
                if state.dirty {
                    // TODO: track per-output dirty flags to avoid re-rendering
                    // outputs that haven't changed.
                    for output in &state.outputs {
                        let frame = Arc::new(renderer::render(output, &state));
                        let _ = frame_sender.send(frame).await;
                    }
                    let timestamp_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                    fire_frame_callbacks(&mut state, timestamp_ms).await;
                    state.dirty = false;
                }
            }
            () = cancel_token.cancelled() => {
                info!("Compositor received shutdown signal");
                break;
            }
        }
    }

    Ok(())
}

/// Fire all pending frame callbacks, presentation feedbacks, and release buffers.
async fn fire_frame_callbacks(state: &mut CompositorState, timestamp_ms: u32) {
    let mut callbacks: Vec<(u32, u32)> = Vec::new(); // (client_id, callback_id)
    let mut presentation: Vec<(u32, u32)> = Vec::new(); // (client_id, feedback_id)
    let mut buffers_to_release: Vec<(u32, u32)> = Vec::new(); // (client_id, buffer_id)

    for surface in state.surfaces.values_mut() {
        if let Some(callback_id) = surface.frame_callback.take() {
            callbacks.push((surface.client_id, callback_id));
        }
        for feedback_id in surface.presentation_feedbacks.drain(..) {
            presentation.push((surface.client_id, feedback_id));
        }
    }

    // Drain buffers that were replaced by a commit, but skip any buffer
    // that is still attached to a surface (can happen when multiple commits
    // are batched and a buffer is re-attached after being replaced).
    for (client_id, buffer_id) in state.buffers_pending_release.drain(..) {
        let still_attached = state
            .surfaces
            .values()
            .any(|s| s.client_id == client_id && s.buffer_id == Some(buffer_id));
        if !still_attached {
            buffers_to_release.push((client_id, buffer_id));
        }
    }

    // Fire wl_callback.done events with timestamp
    for (client_id, callback_id) in callbacks {
        if let Some(client) = state.clients.get(client_id) {
            let args = ArgWriter::new().u32(timestamp_ms).build();
            let _ = client.send(message(callback_id, 0, args)).await;
            client.unregister(callback_id).await;
        } else {
            debug!(
                "Client {} disappeared before frame callback could be fired",
                client_id
            );
        }
    }

    // Fire wp_presentation_feedback.presented events
    if !presentation.is_empty() {
        // Get CLOCK_MONOTONIC timestamp in nanoseconds
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
        let tv_sec_hi = (ts.tv_sec.cast_unsigned() >> 32) as u32;
        let tv_sec_lo = u32::try_from(ts.tv_sec).unwrap_or(0);
        let tv_nsec = u32::try_from(ts.tv_nsec).unwrap_or(0);

        // wp_presentation_feedback.presented event (opcode 0)
        // flags: 0x1 = WP_PRESENTATION_FEEDBACK_KIND_VSYNC
        let flags: u32 = 0x1;
        for (client_id, feedback_id) in presentation {
            let args = ArgWriter::new()
                .u32(tv_sec_hi)
                .u32(tv_sec_lo)
                .u32(tv_nsec)
                .u32(0) // refresh (unused at the moment since we're not doing adaptive sync)
                .u32(0) // seq_hi
                .u32(0) // seq_lo
                .u32(flags)
                .build();
            if let Some(client) = state.clients.get(client_id) {
                let _ = client.send(message(feedback_id, 0, args)).await;
                client.unregister(feedback_id).await;
            } else {
                debug!(
                    "Client {} disappeared before presentation feedback could be fired",
                    client_id
                );
            }
        }
    }

    // Send wl_buffer.release (opcode 0, no args)
    for (client_id, buffer_id) in buffers_to_release {
        if let Some(client) = state.clients.get(client_id) {
            let _ = client.send(message(buffer_id, 0, Vec::new())).await;
        } else {
            debug!(
                "Client {} disappeared before buffer release could be sent",
                client_id
            );
        }
    }
}

/// Switch keyboard focus to a new toplevel surface. Sends keyboard enter/leave
/// and `xdg_toplevel` activated/deactivated configure events. Pointer focus is
/// tracked separately via `state.pointer_surface`.
pub async fn switch_focus(state: &mut protocol::CompositorState, new_key: ClientObjectId) {
    let old_key = state.focused_surface;
    if old_key == Some(new_key) {
        return;
    }

    // Send keyboard leave and deactivate the old focused surface
    if let Some(old_key) = old_key {
        let old_client = old_key.0;
        let old_surface = old_key.1;
        for kb in state.keyboards.clone() {
            if kb.client_id == old_client {
                wl_keyboard::send_leave(state, old_client, kb.object_id, old_surface).await;
            }
        }
        xdg_toplevel::send_activated(state, old_client, old_surface, false).await;
    }

    // Send keyboard enter and activate the new focused surface
    state.focused_surface = Some(new_key);
    let new_client = new_key.0;
    let new_surface = new_key.1;
    for kb in state.keyboards.clone() {
        if kb.client_id == new_client {
            wl_keyboard::send_enter(state, new_client, kb.object_id, new_surface).await;
        }
    }
    xdg_toplevel::send_activated(state, new_client, new_surface, true).await;
}

/// Result of a hit test: which toplevel was hit, and which specific surface
/// (possibly a subsurface) the pointer is actually over.
struct HitResult {
    /// The toplevel surface key (in `surface_stack`) — used for stacking/keyboard focus.
    toplevel: ClientObjectId,
    /// The specific surface under the pointer (could be a subsurface) — used for pointer events.
    surface: ClientObjectId,
    /// Global position of the specific surface, for computing local coordinates.
    surface_x: i32,
    surface_y: i32,
}

/// Hit-test the surface stack from top to bottom. Returns the toplevel and the
/// specific surface (possibly a subsurface) under the pointer.
fn hit_test(state: &protocol::CompositorState, x: f64, y: f64) -> Option<HitResult> {
    let px = x as i32;
    let py = y as i32;

    for &key in state.surface_stack.iter().rev() {
        let Some(surface) = state.surfaces.get(&key) else {
            continue;
        };
        let (ox, oy) = surface.position;

        if let Some((surface_key, sx, sy)) = hit_test_surface_tree(state, key, ox, oy, px, py) {
            return Some(HitResult {
                toplevel: key,
                surface: surface_key,
                surface_x: sx,
                surface_y: sy,
            });
        }
    }
    None
}

/// Recursively hit-test a surface and its children at the given offset.
/// Returns the specific surface key and its global offset if hit.
fn hit_test_surface_tree(
    state: &protocol::CompositorState,
    surface_key: ClientObjectId,
    offset_x: i32,
    offset_y: i32,
    px: i32,
    py: i32,
) -> Option<(ClientObjectId, i32, i32)> {
    let surface = state.surfaces.get(&surface_key)?;
    let client_id = surface.client_id;
    let children = surface.children.clone();

    // Check children first (they render on top of the parent)
    for &child_id in children.iter().rev() {
        let child_key = (client_id, child_id);
        let Some(child) = state.surfaces.get(&child_key) else {
            continue;
        };
        let (cx, cy) = child.subsurface_position;
        if let Some(result) =
            hit_test_surface_tree(state, child_key, offset_x + cx, offset_y + cy, px, py)
        {
            return Some(result);
        }
    }

    // Check this surface's own bounds
    let (w, h) = surface_dimensions(state, surface_key);
    if w == 0 || h == 0 {
        return None;
    }
    if px >= offset_x && py >= offset_y && px < offset_x + w && py < offset_y + h {
        Some((surface_key, offset_x, offset_y))
    } else {
        None
    }
}

/// Get the pixel dimensions of a surface from its buffer (or viewport destination).
fn surface_dimensions(state: &protocol::CompositorState, key: ClientObjectId) -> (i32, i32) {
    let Some(surface) = state.surfaces.get(&key) else {
        return (0, 0);
    };
    let client_id = surface.client_id;

    // Check for viewport destination override
    if let Some(vp) = state
        .surface_viewport
        .get(&(client_id, key.1))
        .and_then(|&vp_id| state.viewports.get(&(client_id, vp_id)))
        && let Some((dw, dh)) = vp.destination
    {
        return (dw, dh);
    }

    // Fall back to buffer dimensions
    let Some(buffer_id) = surface.buffer_id else {
        return (0, 0);
    };
    let Some(buf) = state.shm_buffers.get(&(client_id, buffer_id)) else {
        return (0, 0);
    };
    (buf.width, buf.height)
}

/// Check if the pointer is outside the topmost grabbed popup. If so, dismiss
/// popups from the top of the grab stack until we reach one that contains the
/// pointer (or the stack is empty). Returns true if any popup was dismissed.
async fn dismiss_popups_outside_click(state: &mut CompositorState) -> bool {
    let px = state.cursor_x as i32;
    let py = state.cursor_y as i32;
    let mut dismissed = false;

    while let Some(&(client_id, popup_id)) = state.grabbed_popups.last() {
        // Find the popup's wl_surface and compute its global position
        let popup_surface = state
            .xdg_popups
            .get(&(client_id, popup_id))
            .and_then(|p| state.xdg_surfaces.get(&(client_id, p.xdg_surface_id)))
            .map(|xs| xs.wl_surface_id);

        let Some(wl_surface_id) = popup_surface else {
            state.grabbed_popups.pop();
            continue;
        };

        // Walk up the parent chain to compute global position
        let global_pos = surface_global_position(state, client_id, wl_surface_id);
        let (w, h) = surface_dimensions(state, (client_id, wl_surface_id));

        if px >= global_pos.0
            && py >= global_pos.1
            && px < global_pos.0 + w
            && py < global_pos.1 + h
        {
            // Click is inside this popup — stop dismissing
            break;
        }

        // Click is outside — dismiss this popup
        state.grabbed_popups.pop();
        xdg_popup::send_popup_done(state, client_id, popup_id).await;
        dismissed = true;
    }

    dismissed
}

/// Compute the global position of a surface by walking up the parent chain.
fn surface_global_position(state: &CompositorState, client_id: u32, surface_id: u32) -> (i32, i32) {
    let mut x = 0i32;
    let mut y = 0i32;
    let mut current = surface_id;

    loop {
        let Some(surface) = state.surfaces.get(&(client_id, current)) else {
            break;
        };
        x += surface.subsurface_position.0;
        y += surface.subsurface_position.1;
        if let Some(parent_id) = surface.parent {
            current = parent_id;
        } else {
            // Root surface — add its global position
            x += surface.position.0;
            y += surface.position.1;
            break;
        }
    }

    (x, y)
}
