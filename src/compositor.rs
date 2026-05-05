//! Compositor event loop.
//!
//! Receives messages from two sources: Wayland clients (protocol requests)
//! and the display backend (input events, resize, focus). Composites surfaces
//! into frames and sends them to the backend for display on a 60fps timer.

use std::time::{Duration, Instant};

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
    frame_sender: Sender<RenderFrame>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    info!("Running compositor...");

    let mut state = protocol::CompositorState::new();
    let mut output_width: u32 = 800;
    let mut output_height: u32 = 600;
    let mut render_timer = tokio::time::interval(FRAME_INTERVAL);
    let mut dirty = true; // Start dirty to send initial frame
    let start_time = Instant::now();

    loop {
        tokio::select! {
            Some(message) = wayland_message_receiver.recv() => {
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
                    BackendMessage::KeyInput { keycode, keysym: _, state: key_state } => {
                        let time_ms = start_time.elapsed().as_millis() as u32;
                        let pressed = matches!(key_state, KeyState::Pressed);
                        // Send to all keyboards (single-surface focus for now)
                        for kb in state.keyboards.clone() {
                            wl_keyboard::send_key(&mut state, kb.client_id, kb.object_id, time_ms, keycode - 8, pressed).await;
                        }
                    }
                    BackendMessage::MouseMove { x, y } => {
                        let time_ms = start_time.elapsed().as_millis() as u32;
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
                                // axis 0 = vertical
                                wl_pointer::send_axis(&mut state, ptr.client_id, ptr.object_id, time_ms, 0, dy * 10.0).await;
                            }
                            if dx != 0.0 {
                                // axis 1 = horizontal
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
                    let frame = renderer::render(&state, output_width, output_height);
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

/// Fire all pending frame callbacks and release buffers we're done with.
async fn fire_frame_callbacks(state: &mut protocol::CompositorState, timestamp_ms: u32) {
    let mut callbacks: Vec<(u32, u32)> = Vec::new(); // (client_id, callback_id)
    let mut buffers_to_release: Vec<(u32, u32)> = Vec::new(); // (client_id, buffer_id)

    for surface in state.surfaces.values_mut() {
        if let Some(callback_id) = surface.frame_callback.take() {
            callbacks.push((surface.client_id, callback_id));
        }
        // Release the buffer we just rendered from
        if let Some(buffer_id) = surface.buffer_id {
            buffers_to_release.push((surface.client_id, buffer_id));
        }
    }

    // Fire wl_callback.done events with timestamp
    for (client_id, callback_id) in callbacks {
        let client = state.clients.get_or_create(client_id);
        let args = ArgWriter::new().u32(timestamp_ms).build();
        let _ = client.send(message(callback_id, 0, args)).await;
        client.unregister(callback_id);
    }

    // Send wl_buffer.release (opcode 0, no args)
    for (client_id, buffer_id) in buffers_to_release {
        let client = state.clients.get_or_create(client_id);
        let _ = client.send(message(buffer_id, 0, Vec::new())).await;
    }
}
