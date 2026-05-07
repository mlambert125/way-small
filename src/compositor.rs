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
use crate::protocol;
use crate::protocol::state::{ClientObjectId, OutputState};
use crate::protocol::wire::{ArgWriter, message};
use crate::protocol::{wl_keyboard, wl_pointer};
use crate::renderer;
use crate::wayland_socket::WaylandSocketMessage;

const FRAME_INTERVAL: Duration = Duration::from_millis(16); // ~60fps

pub async fn run_compositor(
    mut wayland_message_receiver: Receiver<WaylandSocketMessage>,
    mut backend_message_receiver: Receiver<BackendMessage>,
    frame_sender: Sender<Arc<RenderFrame>>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    info!("Running compositor...");

    let mut state = protocol::CompositorState::new();
    let mut output_width: u32 = 800;
    let mut output_height: u32 = 600;
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
                    BackendMessage::OutputInfo { width, height, refresh_mhz } => {
                        info!("Output: {}x{} @{}mHz", width, height, refresh_mhz);
                        output_width = width;
                        output_height = height;
                        state.output = Some(OutputState { width, height, refresh_mhz });
                    }
                    BackendMessage::Closed => {
                        info!("Backend requested shutdown");
                        cancel_token.cancel();
                        break;
                    }
                    BackendMessage::Resized(w, h) => {
                        info!("Backend resized to {}x{}", w, h);
                        output_width = w;
                        output_height = h;
                        if let Some(ref mut output) = state.output {
                            output.width = w;
                            output.height = h;
                        }
                        protocol::wl_output::broadcast_mode(&mut state).await;
                        state.dirty = true;
                    }
                    BackendMessage::KeyInput { keycode, state: key_state, mods_depressed, mods_latched, mods_locked, mods_group } => {
                        let time_ms = start_time.elapsed().as_millis() as u32;
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

                    // TODO: Mouse Events aren't really grouped/framed at this point, and probably should be.
                    // (See wl_pointer::frame event)
                    BackendMessage::MouseMove { x, y } => {
                        state.cursor_x = x;
                        state.cursor_y = y;
                        state.dirty = true;
                        let time_ms = start_time.elapsed().as_millis() as u32;

                        // Auto-focus the top surface if nothing is focused yet
                        if state.focused_surface.is_none() &&
                           let Some(&top_key) = state.surface_stack.last() &&
                           state.surfaces.contains_key(&top_key) {
                            switch_focus(&mut state, top_key).await;
                        }

                        let (focused_client, local_x, local_y) = if let Some(focused_key) = state.focused_surface {
                            let (sx, sy) = state.surfaces.get(&focused_key).map(|s| s.position).unwrap_or((0, 0));
                            (Some(focused_key.0), x - sx as f64, y - sy as f64)
                        } else {
                            (None, x, y)
                        };
                        for ptr in state.pointers.clone() {
                            if Some(ptr.client_id) == focused_client {
                                wl_pointer::send_motion(&mut state, ptr.client_id, ptr.object_id, time_ms, local_x, local_y).await;
                                wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id).await;
                            }
                        }
                    }
                    BackendMessage::MouseButton { button, state: btn_state } => {
                        let time_ms = start_time.elapsed().as_millis() as u32;
                        let pressed = matches!(btn_state, crate::backend::ButtonState::Pressed);
                        // Linux evdev button codes
                        let linux_button = match button {
                            MouseButton::Left => 0x110,
                            MouseButton::Right => 0x111,
                            MouseButton::Middle => 0x112,
                        };

                        // On press, hit-test and raise/focus the clicked surface
                        let cx = state.cursor_x;
                        let cy = state.cursor_y;
                        if pressed && let Some(clicked) = hit_test(&state, cx, cy) && state.focused_surface != Some(clicked) {
                            // Raise to top of stack
                            state.surface_stack.retain(|k| *k != clicked);
                            state.surface_stack.push(clicked);
                            state.dirty = true;

                            switch_focus(&mut state, clicked).await;
                        }

                        // Send button event to focused client
                        let focused_client = state.focused_surface.map(|(cid, _)| cid);
                        for ptr in state.pointers.clone() {
                            if Some(ptr.client_id) == focused_client {
                                wl_pointer::send_button(&mut state, ptr.client_id, ptr.object_id, time_ms, linux_button, pressed).await;
                                wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id).await;
                            }
                        }
                    }
                    BackendMessage::MouseScroll { dx, dy } => {
                        let time_ms = start_time.elapsed().as_millis() as u32;
                        let focused_client = state.focused_surface.map(|(cid, _)| cid);
                        for ptr in state.pointers.clone() {
                            if Some(ptr.client_id) != focused_client {
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
                    let frame = Arc::new(renderer::render(&state, output_width, output_height));
                    let timestamp_ms = start_time.elapsed().as_millis() as u32;
                    fire_frame_callbacks(&mut state, timestamp_ms).await;
                    if frame_sender.send(frame).await.is_err() {
                        break;
                    }
                    state.dirty = false;
                }
            }
            _ = cancel_token.cancelled() => {
                info!("Compositor received shutdown signal");
                break;
            }
        }
    }

    Ok(())
}

/// Fire all pending frame callbacks, presentation feedbacks, and release buffers.
async fn fire_frame_callbacks(state: &mut protocol::CompositorState, timestamp_ms: u32) {
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
        let client = state.clients.get_or_create(client_id);
        let args = ArgWriter::new().u32(timestamp_ms).build();
        let _ = client.send(message(callback_id, 0, args)).await;
        client.unregister(callback_id).await;
    }

    // Fire wp_presentation_feedback.presented events
    if !presentation.is_empty() {
        // Get CLOCK_MONOTONIC timestamp in nanoseconds
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        let tv_sec_hi = (ts.tv_sec as u64 >> 32) as u32;
        let tv_sec_lo = ts.tv_sec as u32;
        let tv_nsec = ts.tv_nsec as u32;

        // Refresh interval in nanoseconds (from output refresh rate)
        let refresh_nsec = state
            .output
            .as_ref()
            .and_then(|o| {
                let hz = o.refresh_mhz / 1000;
                if hz > 0 { Some(1_000_000_000u32 / hz) } else { None }
            })
            .unwrap_or(16_666_666); // fallback ~60Hz

        // wp_presentation_feedback.presented event (opcode 0)
        // flags: 0x1 = WP_PRESENTATION_FEEDBACK_KIND_VSYNC
        let flags: u32 = 0x1;
        for (client_id, feedback_id) in presentation {
            let args = ArgWriter::new()
                .u32(tv_sec_hi)
                .u32(tv_sec_lo)
                .u32(tv_nsec)
                .u32(refresh_nsec)
                .u32(0) // seq_hi
                .u32(0) // seq_lo
                .u32(flags)
                .build();
            let client = state.clients.get_or_create(client_id);
            let _ = client.send(message(feedback_id, 0, args)).await;
            client.unregister(feedback_id).await;
        }
    }

    // Send wl_buffer.release (opcode 0, no args)
    for (client_id, buffer_id) in buffers_to_release {
        let client = state.clients.get_or_create(client_id);
        let _ = client.send(message(buffer_id, 0, Vec::new())).await;
    }
}

/// Send leave events to the old focused surface (if any) and enter events to
/// the new one, then update `state.focused_surface`. Coordinates are converted
/// to surface-local automatically.
pub async fn switch_focus(
    state: &mut protocol::CompositorState,
    new_key: ClientObjectId,
) {
    let old_key = state.focused_surface;
    if old_key == Some(new_key) {
        return;
    }

    // Send leave events to old focused surface
    if let Some(old_key) = old_key {
        let old_client = old_key.0;
        let old_surface = old_key.1;
        for ptr in state.pointers.clone() {
            if ptr.client_id == old_client {
                wl_pointer::send_leave(state, ptr.client_id, ptr.object_id, old_surface).await;
                wl_pointer::send_frame(state, ptr.client_id, ptr.object_id).await;
            }
        }
        for kb in state.keyboards.clone() {
            if kb.client_id == old_client {
                wl_keyboard::send_leave(state, kb.client_id, kb.object_id, old_surface).await;
            }
        }
    }

    // Send enter events to new focused surface
    state.focused_surface = Some(new_key);
    let new_client = new_key.0;
    let new_surface = new_key.1;
    let (sx, sy) = state.surfaces.get(&new_key).map(|s| s.position).unwrap_or((0, 0));
    let local_x = state.cursor_x - sx as f64;
    let local_y = state.cursor_y - sy as f64;
    for ptr in state.pointers.clone() {
        if ptr.client_id == new_client {
            wl_pointer::send_enter(state, ptr.client_id, ptr.object_id, new_surface, local_x, local_y).await;
            wl_pointer::send_frame(state, ptr.client_id, ptr.object_id).await;
        }
    }
    for kb in state.keyboards.clone() {
        if kb.client_id == new_client {
            wl_keyboard::send_enter(state, kb.client_id, kb.object_id, new_surface).await;
        }
    }
}

/// Hit-test the surface stack from top to bottom, returning the first surface
/// that contains the given point. Uses the buffer dimensions as the surface size.
fn hit_test(state: &protocol::CompositorState, x: f64, y: f64) -> Option<ClientObjectId> {
    let px = x as i32;
    let py = y as i32;

    // Iterate from top (last) to bottom (first)
    for &key in state.surface_stack.iter().rev() {
        let Some(surface) = state.surfaces.get(&key) else {
            continue;
        };
        let (sx, sy) = surface.position;

        // Get surface dimensions from its attached buffer
        let (w, h) = surface_dimensions(state, key);
        if w == 0 || h == 0 {
            continue;
        }

        if px >= sx && py >= sy && px < sx + w && py < sy + h {
            return Some(key);
        }
    }
    None
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
