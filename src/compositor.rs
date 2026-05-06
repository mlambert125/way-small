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
use crate::protocol::state::OutputState;
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
    let mut dirty = true;
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
                            dirty = true;
                        }
                        WaylandSocketMessage::ClientDisconnected { client_id } => {
                            info!("Client {} disconnected", client_id);
                            state.remove_client_resources(client_id);
                            state.clients.remove(client_id);
                            dirty = true;
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
                        dirty = true;
                    }
                    BackendMessage::KeyInput { keycode, state: key_state, mods_depressed, mods_latched, mods_locked, mods_group } => {
                        let time_ms = start_time.elapsed().as_millis() as u32;
                        let pressed = matches!(key_state, KeyState::Pressed);
                        // Send to all keyboards (single-surface focus for now)
                        for kb in state.keyboards.clone() {
                            wl_keyboard::send_key(&mut state, kb.client_id, kb.object_id, time_ms, keycode - 8, pressed).await;
                            wl_keyboard::send_modifiers(&mut state, kb.client_id, kb.object_id, mods_depressed, mods_latched, mods_locked, mods_group).await;
                        }
                    }

                    // TODO: Mouse Events aren't really grouped/framed at this point, and probably should be.
                    // (See wl_pointer::frame event)
                    BackendMessage::MouseMove { x, y } => {
                        state.cursor_x = x;
                        state.cursor_y = y;
                        dirty = true;
                        let time_ms = start_time.elapsed().as_millis() as u32;
                        // TODO: track which surface(s) the cursor is over to set focus (focus on
                        // hover, alternatively do this in MouseButton for focus on click)

                        // Auto-focus: find first surface and enter it if not already focused
                        if state.focused_surface.is_none() && let Some((&surface_id, surface_client_id)) = state.surfaces.iter().next().map(|(id, s)| (id, s.client_id)) {
                            state.focused_surface = Some(surface_id);
                            for ptr in state.pointers.clone() {
                                if ptr.client_id == surface_client_id {
                                    wl_pointer::send_enter(&mut state, ptr.client_id, ptr.object_id, surface_id, x, y).await;
                                    wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id).await;
                                }
                            }
                            for kb in state.keyboards.clone() {
                                if kb.client_id == surface_client_id {
                                    wl_keyboard::send_enter(&mut state, kb.client_id, kb.object_id, surface_id).await;
                                }
                            }
                        }
                        for ptr in state.pointers.clone() {
                            wl_pointer::send_motion(&mut state, ptr.client_id, ptr.object_id, time_ms, x, y).await;
                            wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id).await;
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
                        for ptr in state.pointers.clone() {
                            wl_pointer::send_button(&mut state, ptr.client_id, ptr.object_id, time_ms, linux_button, pressed).await;
                            wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id).await;
                        }
                    }
                    BackendMessage::MouseScroll { dx, dy } => {
                        let time_ms = start_time.elapsed().as_millis() as u32;
                        for ptr in state.pointers.clone() {
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
                if dirty {
                    let frame = Arc::new(renderer::render(&state, output_width, output_height));
                    let timestamp_ms = start_time.elapsed().as_millis() as u32;
                    fire_frame_callbacks(&mut state, timestamp_ms).await;
                    if frame_sender.send(frame).await.is_err() {
                        break;
                    }
                    dirty = false;
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
            .any(|s| s.buffer_id == Some(buffer_id));
        if !still_attached {
            buffers_to_release.push((client_id, buffer_id));
        }
    }

    // Fire wl_callback.done events with timestamp
    for (client_id, callback_id) in callbacks {
        let client = state.clients.get_or_create(client_id);
        let args = ArgWriter::new().u32(timestamp_ms).build();
        let _ = client.send(message(callback_id, 0, args)).await;
        client.unregister(callback_id);
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
            .map(|o| 1_000_000_000u32 / (o.refresh_mhz / 1000))
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
            client.unregister(feedback_id);
        }
    }

    // Send wl_buffer.release (opcode 0, no args)
    for (client_id, buffer_id) in buffers_to_release {
        let client = state.clients.get_or_create(client_id);
        let _ = client.send(message(buffer_id, 0, Vec::new())).await;
    }
}
