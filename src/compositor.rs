//! Compositor event loop.
//!
//! Receives messages from two sources: Wayland clients (protocol requests)
//! and the display backend (input events, resize, focus). Composites surfaces
//! into frames and sends them to the backend for display on a 60fps timer.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use std::sync::Arc;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::backend::{BackendMessage, KeyState, MouseButton, RenderFrame};
use crate::protocol::state::{ClientObjectId, OutputId, region_contains};
use crate::protocol::wire_utils::{ArgWriter, f64_to_i32, message};
use crate::protocol::{self, CompositorState};
use crate::protocol::{wl_keyboard, wl_pointer, wl_registry, wl_surface, xdg_popup, xdg_toplevel};
use crate::renderer;
use crate::wayland_socket::WaylandSocketMessage;

const FRAME_INTERVAL: Duration = Duration::from_millis(16); // ~60fps

// Keys the compositor binds for itself, as evdev keycodes
// (`linux/input-event-codes.h`) — the same currency as `CompositorState::pressed_keys`
// and `wl_keyboard.key`.
const KEY_TAB: u32 = 15;
const KEY_LEFTALT: u32 = 56;
const KEY_F4: u32 = 62;
const KEY_RIGHTALT: u32 = 100;

/// A key combination the compositor acts on itself rather than forwarding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binding {
    /// Alt+F4 — ask the focused toplevel to close.
    CloseWindow,
    /// Alt+Tab — move focus to the next toplevel.
    CycleFocus,
}

/// Match a key press against the compositor's bindings.
///
/// Alt is matched on the physical keys rather than the xkb modifier mask.
/// Everything else in the input path already speaks evdev keycodes, and the
/// compositor builds its keymap from the default names, so there is no
/// remapping to respect yet. If configurable xkb layouts land (see
/// `docs/notes.md`), this should switch to the `mods_depressed` mask so that a
/// remapped Alt still works.
fn match_binding(evdev_key: u32, pressed_keys: &HashSet<u32>) -> Option<Binding> {
    if !pressed_keys.contains(&KEY_LEFTALT) && !pressed_keys.contains(&KEY_RIGHTALT) {
        return None;
    }
    match evdev_key {
        KEY_F4 => Some(Binding::CloseWindow),
        KEY_TAB => Some(Binding::CycleFocus),
        _ => None,
    }
}

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

    // Keys whose press the compositor consumed for a binding. Their release is
    // swallowed too, so a client never sees a release for a key it never saw
    // pressed — which some toolkits treat as a stuck modifier.
    let mut consumed_keys: HashSet<u32> = HashSet::new();

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
                            state.clients.create(
                                msg.client_id,
                                msg.socket_sender,
                                msg.fd_queue,
                                msg.client_cancel_token,
                            );
                        }
                        WaylandSocketMessage::Message(msg) => {
                            debug!(
                                "client {}: object_id={} op_code={}",
                                msg.client_id, msg.message.object_id, msg.message.op_code
                            );
                            protocol::handle_message(&mut state, &msg);
                        }
                        WaylandSocketMessage::ClientDisconnected { client_id } => {
                            info!("Client {} disconnected", client_id);
                            let was_focused = state.focused_surface.map(|(cid, _)| cid) == Some(client_id);
                            state.remove_client_resources(client_id);
                            state.clients.remove(client_id);
                            state.dirty = true;

                            // Focus the next surface down the stack
                            if was_focused && let Some(&new_key) = state.surface_stack.last() {
                                switch_focus(&mut state, new_key);
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
                                wl_registry::broadcast_output_global(&mut state, global_name);
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

                        protocol::wl_output::broadcast_mode(&mut state);
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
                        // Compositor bindings are handled here and never reach the
                        // client, so a bound combination cannot also trigger an
                        // application shortcut.
                        if pressed {
                            if let Some(binding) = match_binding(evdev_key, &state.pressed_keys) {
                                consumed_keys.insert(evdev_key);
                                match binding {
                                    Binding::CloseWindow => close_focused_window(&mut state),
                                    Binding::CycleFocus => cycle_focus(&mut state),
                                }
                                continue;
                            }
                        } else if consumed_keys.remove(&evdev_key) {
                            continue;
                        }

                        // Only send key events to the focused surface's client
                        let focused_client = state.focused_surface.map(|(cid, _)| cid);
                        for kb in state.keyboards.clone() {
                            if Some(kb.client_id) == focused_client {
                                wl_keyboard::send_key(&mut state, kb.client_id, kb.object_id, time_ms, evdev_key, pressed);
                                wl_keyboard::send_modifiers(&mut state, kb.client_id, kb.object_id, mods_depressed, mods_latched, mods_locked, mods_group);
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
                            switch_focus(&mut state, top_key);
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
                                        wl_pointer::send_leave(&mut state, ptr.client_id, ptr.object_id, old_ps.1);
                                        wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id);
                                    }
                                }
                            }
                            state.pointer_surface = new_pointer_surface;
                            if let Some(ref h) = hit {
                                let local_x = x - f64::from(h.surface_x);
                                let local_y = y - f64::from(h.surface_y);
                                for ptr in state.pointers.clone() {
                                    if ptr.client_id == h.surface.0 {
                                        wl_pointer::send_enter(&mut state, ptr.client_id, ptr.object_id, h.surface.1, local_x, local_y);
                                        wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id);
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
                                    wl_pointer::send_motion(&mut state, ptr.client_id, ptr.object_id, time_ms, local_x, local_y);
                                    wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id);
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
                            let dismissed = dismiss_popups_outside_click(&mut state);
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

                            switch_focus(&mut state, hit.toplevel);

                            // Update pointer surface to the specific surface under cursor
                            if state.pointer_surface != Some(hit.surface) {
                                state.pointer_surface = Some(hit.surface);
                                let local_x = cx - f64::from(hit.surface_x);
                                let local_y = cy - f64::from(hit.surface_y);
                                for ptr in state.pointers.clone() {
                                    if ptr.client_id == hit.surface.0 {
                                        wl_pointer::send_enter(&mut state, ptr.client_id, ptr.object_id, hit.surface.1, local_x, local_y);
                                        wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id);
                                    }
                                }
                            }
                        }

                        // Send button event to the pointer surface
                        if let Some(ps) = state.pointer_surface {
                            for ptr in state.pointers.clone() {
                                if ptr.client_id == ps.0 {
                                    wl_pointer::send_button(&mut state, ptr.client_id, ptr.object_id, time_ms, linux_button, pressed);
                                    wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id);
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
                                wl_pointer::send_axis(&mut state, ptr.client_id, ptr.object_id, time_ms, 0, dy * 10.0);
                            }
                            if dx != 0.0 {
                                // mouse axis 1 = horizontal
                                wl_pointer::send_axis(&mut state, ptr.client_id, ptr.object_id, time_ms, 1, dx * 10.0);
                            }
                            wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id);
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
                update_surface_outputs(&mut state);
                if state.dirty {
                    // TODO: track per-output dirty flags to avoid re-rendering
                    // outputs that haven't changed.
                    for output in &state.outputs {
                        let frame = Arc::new(renderer::render(output, &state));
                        let _ = frame_sender.send(frame).await;
                    }
                    let timestamp_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                    fire_frame_callbacks(&mut state, timestamp_ms);
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

/// Tell clients which outputs each of their surfaces is displayed on.
///
/// A client cannot choose a buffer scale without this: `wl_output.scale` is per
/// output, so the surface has to know which outputs it overlaps. The full set is
/// recomputed and diffed against what each surface has already been told, so
/// this is idempotent and safe to run every frame.
fn update_surface_outputs(state: &mut CompositorState) {
    let mut enters: Vec<(u32, u32, OutputId)> = Vec::new();
    let mut leaves: Vec<(u32, u32, OutputId)> = Vec::new();

    for (&(client_id, surface_id), surface) in &state.surfaces {
        // An unmapped surface is on no output.
        if surface.buffer_id.is_none() {
            leaves.extend(
                surface
                    .entered_outputs
                    .iter()
                    .map(|&output_id| (client_id, surface_id, output_id)),
            );
            continue;
        }

        let (x, y) = surface_global_position(state, client_id, surface_id);
        let (w, h) = surface_dimensions(state, (client_id, surface_id));

        for output in &state.outputs {
            let geometry = &output.geometry;
            let overlaps = x < geometry.x + geometry.physical_width
                && x + w > geometry.x
                && y < geometry.y + geometry.physical_height
                && y + h > geometry.y;
            match (overlaps, surface.entered_outputs.contains(&output.id)) {
                (true, false) => enters.push((client_id, surface_id, output.id)),
                (false, true) => leaves.push((client_id, surface_id, output.id)),
                _ => {}
            }
        }
    }

    for (client_id, surface_id, output_id) in enters {
        let objects = bound_output_objects(state, client_id, output_id);
        // A client that has not bound this output has no object we could name,
        // so leave the surface unmarked and try again once it binds.
        if objects.is_empty() {
            continue;
        }
        for object_id in objects {
            wl_surface::send_enter(state, client_id, surface_id, object_id);
        }
        if let Some(surface) = state.surfaces.get_mut(&(client_id, surface_id)) {
            surface.entered_outputs.insert(output_id);
        }
    }

    for (client_id, surface_id, output_id) in leaves {
        for object_id in bound_output_objects(state, client_id, output_id) {
            wl_surface::send_leave(state, client_id, surface_id, object_id);
        }
        if let Some(surface) = state.surfaces.get_mut(&(client_id, surface_id)) {
            surface.entered_outputs.remove(&output_id);
        }
    }
}

/// The client's `wl_output` object ids that refer to a given output. A client
/// may bind the same output more than once, and each binding is a distinct
/// object the events have to be sent for.
fn bound_output_objects(state: &CompositorState, client_id: u32, output_id: OutputId) -> Vec<u32> {
    state
        .output_bindings
        .iter()
        .filter(|&(&(cid, _), &oid)| cid == client_id && oid == output_id)
        .map(|(&(_, object_id), _)| object_id)
        .collect()
}

/// Ask the focused toplevel to close.
///
/// Only a request — the client decides whether to honour it, so the window is
/// torn down later through the normal `xdg_toplevel.destroy` path, not here.
fn close_focused_window(state: &mut CompositorState) {
    let Some((client_id, wl_surface_id)) = state.focused_surface else {
        return;
    };
    let Some((_, toplevel_id)) = xdg_toplevel::xdg_ids_for_surface(state, client_id, wl_surface_id)
    else {
        return;
    };
    info!("Alt+F4: asking client {client_id} to close toplevel {toplevel_id}");
    xdg_toplevel::send_close(state, client_id, toplevel_id);
}

/// Move focus to the next toplevel, raising it to the top of the stack.
///
/// `surface_stack` is ordered bottom to top, so taking the bottom entry and
/// pushing it on top rotates through every window on repeated presses rather
/// than toggling between the most recent two.
fn cycle_focus(state: &mut CompositorState) {
    if state.surface_stack.len() < 2 {
        return;
    }
    let next = state.surface_stack.remove(0);
    state.surface_stack.push(next);
    state.dirty = true;
    switch_focus(state, next);
}

/// Fire all pending frame callbacks, presentation feedbacks, and release buffers.
fn fire_frame_callbacks(state: &mut CompositorState, timestamp_ms: u32) {
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
            let _ = client.send(message(callback_id, 0, args));
            client.unregister(callback_id);
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
                let _ = client.send(message(feedback_id, 0, args));
                client.unregister(feedback_id);
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
            let _ = client.send(message(buffer_id, 0, Vec::new()));
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
pub fn switch_focus(state: &mut protocol::CompositorState, new_key: ClientObjectId) {
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
                wl_keyboard::send_leave(state, old_client, kb.object_id, old_surface);
            }
        }
        xdg_toplevel::send_activated(state, old_client, old_surface, false);
    }

    // Send keyboard enter and activate the new focused surface
    state.focused_surface = Some(new_key);
    let new_client = new_key.0;
    let new_surface = new_key.1;
    for kb in state.keyboards.clone() {
        if kb.client_id == new_client {
            wl_keyboard::send_enter(state, new_client, kb.object_id, new_surface);
        }
    }
    xdg_toplevel::send_activated(state, new_client, new_surface, true);
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
    let px = f64_to_i32(x);
    let py = f64_to_i32(y);

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
    if px < offset_x || py < offset_y || px >= offset_x + w || py >= offset_y + h {
        return None;
    }

    // Within the surface's bounds, but the client may have narrowed which parts
    // of it accept pointer input. Clients drawing their own decorations use this
    // to let clicks fall through the drop shadow around the window.
    if let Some(input_region) = &surface.input_region
        && !region_contains(input_region, px - offset_x, py - offset_y)
    {
        return None;
    }

    Some((surface_key, offset_x, offset_y))
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
    // The buffer is in physical pixels; hit-testing and layout work in
    // surface-local coordinates, which are `buffer_scale` times smaller.
    let scale = surface.buffer_scale.max(1);
    (buf.width / scale, buf.height / scale)
}

/// Check if the pointer is outside the topmost grabbed popup. If so, dismiss
/// popups from the top of the grab stack until we reach one that contains the
/// pointer (or the stack is empty). Returns true if any popup was dismissed.
fn dismiss_popups_outside_click(state: &mut CompositorState) -> bool {
    let px = f64_to_i32(state.cursor_x);
    let py = f64_to_i32(state.cursor_y);
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
        xdg_popup::send_popup_done(state, client_id, popup_id);
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

#[cfg(test)]
mod tests {
    use super::{
        Binding, CompositorState, KEY_F4, KEY_LEFTALT, KEY_RIGHTALT, KEY_TAB, close_focused_window,
        cycle_focus, match_binding,
    };
    use crate::wayland_socket::WaylandProtocolMessage;
    use std::collections::{HashSet, VecDeque};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc::{Receiver, channel};
    use tokio_util::sync::CancellationToken;

    fn held(keys: &[u32]) -> HashSet<u32> {
        keys.iter().copied().collect()
    }

    #[test]
    fn bindings_require_alt() {
        assert_eq!(match_binding(KEY_F4, &held(&[])), None);
        assert_eq!(match_binding(KEY_TAB, &held(&[])), None);
        assert_eq!(
            match_binding(KEY_F4, &held(&[KEY_LEFTALT])),
            Some(Binding::CloseWindow)
        );
        assert_eq!(
            match_binding(KEY_TAB, &held(&[KEY_RIGHTALT])),
            Some(Binding::CycleFocus)
        );
    }

    #[test]
    fn unbound_keys_pass_through_even_with_alt() {
        // Alt+E belongs to the client, not the compositor.
        assert_eq!(match_binding(18, &held(&[KEY_LEFTALT])), None);
    }

    /// Register a client and give back the receiver its socket task would drain.
    fn add_client(state: &mut CompositorState, client_id: u32) -> Receiver<WaylandProtocolMessage> {
        let (tx, rx) = channel(64);
        state.clients.create(
            client_id,
            tx,
            Arc::new(Mutex::new(VecDeque::new())),
            CancellationToken::new(),
        );
        rx
    }

    /// Build a mapped toplevel backed by a `wl_surface`.
    fn add_toplevel(
        state: &mut CompositorState,
        client_id: u32,
        wl_surface_id: u32,
        xdg_surface_id: u32,
        toplevel_id: u32,
    ) {
        state.create_surface(client_id, wl_surface_id);
        state.create_xdg_surface(client_id, xdg_surface_id, wl_surface_id);
        state.create_xdg_toplevel(client_id, toplevel_id, xdg_surface_id);
    }

    #[test]
    fn buffer_scale_shrinks_the_hit_testable_area() {
        use crate::protocol::state::ShmBuffer;
        let mut state = CompositorState::new();
        let _rx = add_client(&mut state, 1);
        state.create_surface(1, 10);
        state.shm_buffers.insert(
            (1, 11),
            ShmBuffer {
                client_id: 1,
                pool_id: 0,
                offset: 0,
                width: 200,
                height: 100,
                stride: 800,
                format: 0,
            },
        );
        let surface = state.surfaces.get_mut(&(1, 10)).unwrap();
        surface.buffer_id = Some(11);

        assert_eq!(super::surface_dimensions(&state, (1, 10)), (200, 100));

        state.surfaces.get_mut(&(1, 10)).unwrap().buffer_scale = 2;
        assert_eq!(
            super::surface_dimensions(&state, (1, 10)),
            (100, 50),
            "a scale-2 buffer covers half as much surface-local area"
        );
    }

    #[test]
    fn close_sends_xdg_toplevel_close_to_the_focused_window() {
        let mut state = CompositorState::new();
        let mut rx = add_client(&mut state, 1);
        add_toplevel(&mut state, 1, 10, 11, 12);
        state.focused_surface = Some((1, 10));

        close_focused_window(&mut state);

        let msg = rx.try_recv().expect("expected an xdg_toplevel.close");
        assert_eq!(
            msg.object_id, 12,
            "addressed to the toplevel, not the surface"
        );
        assert_eq!(msg.op_code, 1, "xdg_toplevel.close");
    }

    #[test]
    fn close_with_nothing_focused_is_a_no_op() {
        let mut state = CompositorState::new();
        let mut rx = add_client(&mut state, 1);
        add_toplevel(&mut state, 1, 10, 11, 12);
        state.focused_surface = None;

        close_focused_window(&mut state);

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn cycle_rotates_through_every_window() {
        let mut state = CompositorState::new();
        let _rx = add_client(&mut state, 1);
        add_toplevel(&mut state, 1, 10, 11, 12);
        add_toplevel(&mut state, 1, 20, 21, 22);
        add_toplevel(&mut state, 1, 30, 31, 32);
        // create_xdg_toplevel pushes each new window on top, bottom-to-top.
        assert_eq!(state.surface_stack, vec![(1, 10), (1, 20), (1, 30)]);

        cycle_focus(&mut state);
        assert_eq!(state.focused_surface, Some((1, 10)));
        assert_eq!(state.surface_stack, vec![(1, 20), (1, 30), (1, 10)]);

        cycle_focus(&mut state);
        assert_eq!(state.focused_surface, Some((1, 20)));

        cycle_focus(&mut state);
        assert_eq!(state.focused_surface, Some((1, 30)));
    }

    #[test]
    fn cycle_with_one_window_does_nothing() {
        let mut state = CompositorState::new();
        let _rx = add_client(&mut state, 1);
        add_toplevel(&mut state, 1, 10, 11, 12);
        state.focused_surface = Some((1, 10));

        cycle_focus(&mut state);

        assert_eq!(state.surface_stack, vec![(1, 10)]);
        assert_eq!(state.focused_surface, Some((1, 10)));
    }
}
