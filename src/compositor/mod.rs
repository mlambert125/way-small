//! Compositor subsystem.
//!
//! Owns all compositor state and the event loop that drives it. Receives
//! messages from two sources: Wayland clients (protocol requests) and the
//! display backend (input events, resize, focus).
//!
//! Rasterises nothing itself. On a 60fps timer it builds a scene of textured
//! quads per output — see [`scene`] — and publishes it to the backend, which
//! turns it into GPU work.

use crate::shared::{BackendMessage, BackendRequest, DmabufProbe, Frame, KeyState, MouseButton};
use crate::shared::{OutputId, PresentedAt, Scene, ScrollSource, fourcc_name, output_contains};
use crate::wayland_socket::WaylandSocketMessage;
use protocol::CompositorState;
use protocol::state::{ClientObjectId, GrabKind, OfferKind, ResizeEdges, region_contains};
use protocol::wire_utils::{ArgWriter, f64_to_i32, message};
use protocol::{
    wl_data_device, wl_data_offer, wl_data_source, wl_keyboard, wl_pointer, wl_registry, wl_seat,
    wl_surface, wl_touch, wp_presentation_feedback, xdg_popup, xdg_surface, xdg_toplevel,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Receiver;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub mod protocol;
pub mod scene;
#[cfg(test)]
mod tests;
pub mod workspace;

/// How often the compositor does the work that no display paces.
///
/// This is not frame pacing. Rendering is driven by the backend asking for a
/// frame on a particular output — see [`BackendMessage::FrameRequested`] — and
/// the compositor has no opinion on when a display can take one. What is left
/// here is upkeep that has to happen whether or not anything is on screen:
/// noticing that a buffer has stopped being read, re-confining windows to
/// their outputs, and pacing the clients whose surfaces no display is showing.
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(16);

// evdev keycode for TAB (used for now for hard-coded keymap)
const KEY_TAB: u32 = 15;
// evdev keycode for LEFTALT (used for now for hard-coded keymap)
const KEY_LEFTALT: u32 = 56;
// evdev keycode for F4 (used for now for hard-coded keymap)
const KEY_F4: u32 = 62;
// evdev keycode for RIGHTALT (used for now for hard-coded keymap)
const KEY_RIGHTALT: u32 = 100;
// evdev keycode for ESC, which abandons a drag
const KEY_ESC: u32 = 1;

/// Surface-local distance one unit of scroll moves.
///
/// A wheel detent arrives as one line and clients expect rather more than one
/// pixel from it. The protocol has no unit of its own here — the value is
/// whatever the compositor says it is — so this is a feel setting rather than a
/// conversion.
const SCROLL_STEP: f64 = 10.0;

/// Smallest window width an interactive resize will produce.
const MIN_WINDOW_WIDTH: i32 = 120;
/// Smallest window height an interactive resize will produce.
const MIN_WINDOW_HEIGHT: i32 = 80;

/// A key combination the compositor acts on itself rather than forwarding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binding {
    /// Alt+F4 — ask the focused toplevel to close.
    CloseWindow,
    /// Alt+Tab — move focus to the next toplevel.
    CycleFocus,
}

/// Result of a hit test: which toplevel was hit, and which specific surface
/// (possibly a subsurface) the pointer is actually over.
struct HitResult {
    /// The toplevel surface key (in its workspace's stack) — used for stacking/keyboard focus.
    toplevel: ClientObjectId,
    /// The specific surface under the pointer (could be a subsurface) — used for pointer events.
    surface: ClientObjectId,
    /// Global x position of the specific surface, for computing local coordinates.
    surface_x: i32,
    /// Global y position of the specific surface, for computing local coordinates.
    surface_y: i32,
}

/// Which outputs are owed a scene, and what each was last given.
///
/// A scene is composed for an output when two things are true at once: the
/// backend has said it can show another frame there, and what that output was
/// last given no longer reflects compositor state. Either half can become true
/// at any time — a page flip completing, a client committing — so the decision
/// is made in one place after every pass of the loop rather than in whichever
/// arm happened to settle the second half.
struct FramePacer {
    /// The newest scene composed for each output.
    ///
    /// Kept so that every publication can carry all of them. The frame slot
    /// holds one value and a new publication replaces it, so a frame carrying
    /// only the output being served would drop the scene of an output whose
    /// backend had not drawn it yet.
    published: HashMap<OutputId, Arc<Scene>>,
    /// Outputs whose published scene is out of date.
    stale: HashSet<OutputId>,
    /// Outputs whose backend has asked for a frame and not been given one.
    ///
    /// A request outlives the moment it was made. An output that asks while
    /// nothing has changed is not turned away — it is served as soon as
    /// something does, which is what keeps an idle desktop from waiting a
    /// whole refresh period to show the first thing that moves.
    waiting: HashSet<OutputId>,
    /// Source of scene serials. Rises forever, so a backend comparing a scene
    /// against what it last drew on that output cannot be fooled by reuse.
    next_serial: u64,
    /// Textures kept between scenes.
    cache: scene::SceneCache,
}

impl FramePacer {
    fn new() -> Self {
        Self {
            published: HashMap::new(),
            stale: HashSet::new(),
            waiting: HashSet::new(),
            next_serial: 1,
            cache: scene::SceneCache::new(),
        }
    }

    /// Note that compositor state has moved on, so every output's scene is out
    /// of date.
    ///
    /// State is tracked as one flag rather than per output because almost
    /// everything that changes it — a commit, a focus change, the pointer
    /// moving — could affect any output, and working out which ones it really
    /// touched costs more than composing a scene nobody was waiting for.
    fn invalidate(&mut self, state: &CompositorState) {
        self.stale.extend(state.outputs.iter().map(|o| o.id));
    }

    /// Record that a backend is ready for another frame on this output.
    fn request(&mut self, output_id: OutputId) {
        self.waiting.insert(output_id);
    }

    /// Drop everything remembered about outputs that no longer exist.
    fn forget_gone_outputs(&mut self, state: &CompositorState) {
        let live: HashSet<OutputId> = state.outputs.iter().map(|o| o.id).collect();
        self.published.retain(|id, _| live.contains(id));
        self.stale.retain(|id| live.contains(id));
        self.waiting.retain(|id| live.contains(id));
    }

    /// Compose for every output that is both waiting and out of date, and
    /// publish the result.
    ///
    /// Never awaited, in line with the rule that the compositor task blocks on
    /// nothing: the slot holds one frame and a backend that has not kept up
    /// gets the newest.
    fn publish(&mut self, state: &mut CompositorState, frames: &watch::Sender<Frame>) {
        if state.dirty {
            self.invalidate(state);
            state.dirty = false;
        }

        let due: Vec<OutputId> = self.stale.intersection(&self.waiting).copied().collect();
        if due.is_empty() {
            return;
        }

        self.cache.gc(state);
        for output_id in due {
            let serial = self.next_serial;
            self.next_serial += 1;
            let scene = Arc::new(scene::build(output_id, serial, state, &mut self.cache));
            self.published.insert(output_id, scene);
            self.stale.remove(&output_id);
            self.waiting.remove(&output_id);
        }

        // Every output draws through one backend and one texture cache, keyed
        // by the buffer's content serial, so a buffer another output has not
        // been composed with yet is either already uploaded or absent — and an
        // absent texture is uploaded whole rather than patched. Carrying the
        // damage forward for that output would therefore change nothing, while
        // an output that stopped asking for frames would grow the list without
        // bound.
        state.clear_buffer_damage();

        let frame: Frame = self.published.values().map(Arc::clone).collect();
        drop(frames.send_replace(frame));
    }
}

/// Whose frame callbacks a presentation settles.
#[derive(Debug, Clone, Copy)]
enum FrameTarget {
    /// The surfaces shown on one output, because that output presented.
    Output(OutputId),
    /// The surfaces no output is showing.
    ///
    /// A client waiting on `wl_surface.frame` for one of these would otherwise
    /// wait forever, and it is not always a window the user has hidden: a
    /// client's first commit typically carries no buffer and a frame request,
    /// and it will not draw anything until that callback comes back.
    Offscreen,
}

/// Run the compositor thread
#[allow(clippy::too_many_lines)]
pub async fn run_compositor(
    mut wayland_message_receiver: Receiver<WaylandSocketMessage>,
    mut backend_message_receiver: Receiver<BackendMessage>,
    backend_requests: tokio::sync::mpsc::Sender<BackendRequest>,
    frame_sender: watch::Sender<Frame>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    info!("Running compositor...");

    // Asked once, up front: the answer decides whether clients are offered
    // dma-buf at all, and the backend may not be able to give it until its own
    // display is up. Nothing waits for it — it arrives as a `BackendMessage`.
    if backend_requests
        .try_send(BackendRequest::ProbeDmabuf)
        .is_err()
    {
        warn!("could not ask the backend about dma-buf support");
    }

    let mut state = protocol::CompositorState::new();
    // Protocol handlers need this too: a client asking for a dma-buf buffer
    // cannot be answered until the backend has tried to import it.
    state.backend_sender = Some(backend_requests.clone());
    state.default_cursor = scene::load_default_cursor();
    if state.default_cursor.is_none() {
        info!("No cursor theme found, using built-in cursor");
    }
    let mut pacer = FramePacer::new();
    // High-water mark for pages the SIGBUS net has blanked, so each new one is
    // reported once.
    let mut reported_patched_pages = 0usize;
    let mut housekeeping_timer = tokio::time::interval(HOUSEKEEPING_INTERVAL);
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
                            if was_focused && let Some(new_key) = state.top_visible_toplevel() {
                                switch_focus(&mut state, new_key);
                            }
                        }
                    }
                }
            }
            Some(message) = backend_message_receiver.recv() => {
                match message {
                    BackendMessage::SeatCapabilities { pointer, keyboard, touch } => {
                        info!(
                            "Seat capabilities: pointer={} keyboard={} touch={}",
                            pointer, keyboard, touch
                        );
                        let changed = state.seat.has_pointer != pointer
                            || state.seat.has_keyboard != keyboard
                            || state.seat.has_touch != touch;
                        state.seat.has_pointer = pointer;
                        state.seat.has_keyboard = keyboard;
                        state.seat.has_touch = touch;
                        // A capability can appear after clients have bound the
                        // seat — a touchscreen is only known to exist once it
                        // is touched — so everyone already connected is told
                        // again rather than only whoever binds next.
                        if changed {
                            wl_seat::broadcast_capabilities(&mut state);
                        }
                    }
                    BackendMessage::OutputInfo { outputs } => {
                        let seen: HashSet<OutputId> =
                            outputs.iter().map(|output| output.id).collect();
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
                                wl_registry::broadcast_global(
                                    &mut state,
                                    global_name,
                                    "wl_output",
                                    protocol::WL_OUTPUT_VERSION,
                                );
                            }
                        }
                        // `OutputInfo` is the whole set the backend can see, so
                        // an output missing from it has gone. Withdrawing the
                        // global is the half that matters to clients: one that
                        // is announced and never withdrawn leaves them holding a
                        // name for a display that no longer exists.
                        let gone: Vec<OutputId> = state
                            .outputs
                            .iter()
                            .map(|output| output.id)
                            .filter(|id| !seen.contains(id))
                            .collect();
                        for output_id in gone {
                            info!("Output {:?} disconnected", output_id);
                            if let Some(global_name) = state.remove_output(output_id) {
                                wl_registry::broadcast_global_remove(&mut state, global_name);
                            }
                        }

                        // A new output arrives with no workspace, and a window
                        // opening before it has one would have nowhere to go.
                        state.sync_workspaces();
                    }
                    BackendMessage::DmabufSupport { formats, probe } => {
                        match probe {
                            DmabufProbe::Passed | DmabufProbe::Untested(_) => {
                                info!(
                                    "backend imports dma-buf: {} format(s), e.g. {}",
                                    formats.len(),
                                    describe_formats(formats.iter().take(4)),
                                );
                                debug!("dma-buf formats: {}", describe_formats(formats.iter()));
                            }
                            DmabufProbe::Unsupported(ref reason) => {
                                info!("backend cannot import dma-buf: {reason}");
                            }
                            DmabufProbe::Failed(ref reason) => {
                                warn!("backend dma-buf import is broken: {reason}");
                            }
                        }
                        // Kept for the protocol layer to advertise from. Empty
                        // means no `zwp_linux_dmabuf_v1` global, which is the
                        // honest answer when nothing can be imported.
                        state.dmabuf_formats = formats;

                        // The global appears the moment there is something
                        // behind it, which may be after clients have connected
                        // — hence the broadcast as well as the listing every
                        // later client gets. Guarded so a second answer cannot
                        // advertise the same interface twice.
                        if !state.dmabuf_formats.is_empty()
                            && state.dmabuf_global_name.is_none()
                        {
                            let global_name = state.next_global_number;
                            state.next_global_number += 1;
                            state.dmabuf_global_name = Some(global_name);
                            wl_registry::broadcast_global(
                                &mut state,
                                global_name,
                                protocol::zwp_linux_dmabuf::INTERFACE,
                                protocol::zwp_linux_dmabuf::VERSION,
                            );
                        }
                    }
                    BackendMessage::DmabufImportResult { token, imported } => {
                        protocol::zwp_linux_buffer_params::resolve_import(
                            &mut state, token, imported,
                        );
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
                        // Escape abandons a drag. Without it the only way out
                        // of one is to drop it somewhere, and the user may have
                        // no somewhere they are willing to drop it on.
                        if pressed && evdev_key == KEY_ESC && state.drag.is_some() {
                            state.cancel_drag();
                            consumed_keys.insert(evdev_key);
                            continue;
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

                    BackendMessage::MouseMovedTo { x, y } => {
                        state.move_cursor_to(x, y);
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        deliver_pointer_motion(&mut state, time_ms);
                    }
                    BackendMessage::MouseMovedBy { dx, dy } => {
                        state.move_cursor_by(dx, dy);
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        deliver_pointer_motion(&mut state, time_ms);
                    }
                    BackendMessage::MouseButton { button, state: btn_state } => {
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        let pressed = matches!(btn_state, crate::shared::ButtonState::Pressed);
                        // Linux evdev button codes
                        let linux_button = match button {
                            MouseButton::Left => 0x110,
                            MouseButton::Right => 0x111,
                            MouseButton::Middle => 0x112,
                        };

                        if pressed {
                            state.pressed_buttons.insert(linux_button);
                        } else {
                            state.pressed_buttons.remove(&linux_button);
                        }

                        // A grab runs until the button comes up, and swallows
                        // everything in between.
                        if state.pointer_grab.is_some() {
                            if !pressed {
                                end_grab(&mut state);
                            }
                            continue;
                        }

                        // So does a drag, and for the same reason: the
                        // compositor owns the pointer, and what the button does
                        // is decide how the drag ends.
                        if state.drag.is_some() {
                            if !pressed {
                                finish_drag(&mut state);
                            }
                            continue;
                        }

                        // Alt+drag moves and resizes without the client's help,
                        // which is the only way for one that draws no usable
                        // decorations.
                        if pressed
                            && alt_held(&state)
                            && let Some(hit) = hit_test(&state, state.cursor_x, state.cursor_y)
                        {
                            raise_window(&mut state, hit.toplevel);
                            switch_focus(&mut state, hit.toplevel);
                            match button {
                                MouseButton::Left => state.start_move_grab(hit.toplevel),
                                MouseButton::Right => {
                                    let edges = edges_for_point(
                                        &state,
                                        hit.toplevel,
                                        state.cursor_x,
                                        state.cursor_y,
                                    );
                                    state.start_resize_grab(hit.toplevel, edges);
                                }
                                MouseButton::Middle => {}
                            }
                            state.dirty = true;
                            continue;
                        }

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
                            // Raise to top of its workspace
                            raise_window(&mut state, hit.toplevel);
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
                                    let serial = wl_pointer::send_button(&mut state, ptr.client_id, ptr.object_id, time_ms, linux_button, pressed);
                                    // Remembered so the client can quote it if
                                    // this press turns into a move or resize.
                                    if pressed {
                                        state.last_button_serial.insert(ptr.client_id, serial);
                                    }
                                    wl_pointer::send_frame(&mut state, ptr.client_id, ptr.object_id);
                                }
                            }
                        }
                    }
                    BackendMessage::TouchDown { id, x, y } => {
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        touch_down(&mut state, time_ms, id, x, y);
                    }
                    BackendMessage::TouchMotion { id, x, y } => {
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        touch_motion(&mut state, time_ms, id, x, y);
                    }
                    BackendMessage::TouchUp { id } => {
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        touch_up(&mut state, time_ms, id);
                    }
                    BackendMessage::TouchCancel => {
                        touch_cancel(&mut state);
                    }
                    BackendMessage::MouseScroll { dx, dy, source, v120_x, v120_y } => {
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        deliver_scroll(&mut state, time_ms, dx, dy, source, v120_x, v120_y);
                    }
                    BackendMessage::MouseScrollEnd => {
                        let time_ms = u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        deliver_scroll_end(&mut state, time_ms);
                    }
                    BackendMessage::FocusIn => {
                        debug!("Focus in");
                    }
                    BackendMessage::FocusOut => {
                        debug!("Focus out");
                    }
                    BackendMessage::FrameRequested(output_id) => {
                        // The backend can show another frame here. Whether one
                        // gets composed is settled at the bottom of the loop,
                        // where both halves of that decision are in view.
                        pacer.request(output_id);
                    }
                    BackendMessage::FramePresented(output_id, presented_at) => {
                        // Clients pace themselves on this, so it follows the
                        // frame reaching the screen rather than the compositor
                        // handing it over. Callbacks live in surface state
                        // until fired, so a frame the backend skipped costs a
                        // client latency but never a lost callback.
                        //
                        // Only the surfaces this output was showing: a client
                        // with a window on each of two displays is paced by
                        // each of them separately, which is the whole point of
                        // the output being named here.
                        let timestamp_ms =
                            u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                        fire_frame_callbacks(
                            &mut state,
                            timestamp_ms,
                            FrameTarget::Output(output_id),
                            Some(presented_at),
                        );
                    }
                }
            }
            _ = housekeeping_timer.tick() => {
                update_surface_outputs(&mut state);
                // Windows belong to one output and stay inside it, so a client
                // resizing itself or an output changing size can move them.
                if state.confine_toplevels() {
                    state.dirty = true;
                }
                pacer.forget_gone_outputs(&state);
                // A bell that has run its course has to be taken off the screen,
                // and nothing else would notice it had expired.
                state.expire_bells();

                // Surfaces no display is showing are paced from here, because
                // nothing else will pace them: no output will ever report
                // presenting them. Their presentation feedback is `discarded`
                // rather than `presented`, which is what it is — nothing
                // reached a screen.
                let timestamp_ms =
                    u32::try_from(start_time.elapsed().as_millis()).unwrap_or(0);
                fire_frame_callbacks(&mut state, timestamp_ms, FrameTarget::Offscreen, None);

                // Both independent of the frame path: a buffer becomes free
                // when the last frame referencing it goes, which can happen on
                // a tick where nothing has changed.
                start_buffer_releases(&mut state);
                finish_buffer_releases(&mut state);

                // The SIGBUS handler cannot log, so the count is reported here.
                let patched = crate::shared::patched_pages();
                if patched > reported_patched_pages {
                    warn!(
                        "{} page(s) of client shm blanked after a pool was truncated \
                         while in use; that client is showing black where it shrank",
                        patched - reported_patched_pages
                    );
                    reported_patched_pages = patched;
                }
            }
            () = cancel_token.cancelled() => {
                info!("Compositor received shutdown signal");
                break;
            }
        }

        // Composing happens here rather than in any one arm. An output is owed
        // a scene when it is both waiting for one and out of date, and those
        // two halves are settled by different messages arriving at different
        // times — a page flip on one side, a client commit on the other. Asking
        // once per pass is what keeps the answer from depending on which of
        // them happened to arrive second.
        pacer.publish(&mut state, &frame_sender);
    }

    Ok(())
}

/// Work out what the pointer is now over, and tell the client about it.
///
/// Reads the position from state rather than taking one, because by this point
/// it has been constrained onto an output and the raw value the backend sent is
/// no longer the truth.
fn deliver_pointer_motion(state: &mut CompositorState, time_ms: u32) {
    state.dirty = true;
    let (x, y) = (state.cursor_x, state.cursor_y);

    // A grab owns the pointer: motion drives the window, and the client hears
    // nothing until the button is up.
    if update_grab(state, x, y) {
        return;
    }

    // A drag owns it for the same reason, and delivers to the surface
    // underneath through its data device rather than its pointer.
    if update_drag(state, time_ms) {
        return;
    }

    // Auto-focus the top surface if nothing is focused yet
    if state.focused_surface.is_none()
        && let Some(top_key) = state.top_visible_toplevel()
        && state.surfaces.contains_key(&top_key)
    {
        switch_focus(state, top_key);
    }

    // Determine which specific surface the pointer is over
    let hit = hit_test(state, x, y);
    let new_pointer_surface = hit.as_ref().map(|h| h.surface);
    let old_pointer_surface = state.pointer_surface;

    // Send pointer enter/leave when the surface under the cursor changes
    if new_pointer_surface != old_pointer_surface {
        if let Some(old_ps) = old_pointer_surface {
            for ptr in state.pointers.clone() {
                if ptr.client_id == old_ps.0 {
                    wl_pointer::send_leave(state, ptr.client_id, ptr.object_id, old_ps.1);
                    wl_pointer::send_frame(state, ptr.client_id, ptr.object_id);
                }
            }
        }
        state.pointer_surface = new_pointer_surface;
        if let Some(ref h) = hit {
            let local_x = x - f64::from(h.surface_x);
            let local_y = y - f64::from(h.surface_y);
            for ptr in state.pointers.clone() {
                if ptr.client_id == h.surface.0 {
                    wl_pointer::send_enter(
                        state,
                        ptr.client_id,
                        ptr.object_id,
                        h.surface.1,
                        local_x,
                        local_y,
                    );
                    wl_pointer::send_frame(state, ptr.client_id, ptr.object_id);
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
                wl_pointer::send_motion(
                    state,
                    ptr.client_id,
                    ptr.object_id,
                    time_ms,
                    local_x,
                    local_y,
                );
                wl_pointer::send_frame(state, ptr.client_id, ptr.object_id);
            }
        }
    }
}

/// Raise the window under a click, bringing any dialogs of its own with it.
///
/// Goes through the toplevel rather than straight to the workspace stack so
/// that `set_parent` is honoured — a dialog must not be buried by a click on
/// the window it belongs to.
fn raise_window(state: &mut CompositorState, surface: ClientObjectId) {
    match state.toplevel_for_surface(surface) {
        Some(toplevel) => state.raise_with_children(toplevel),
        None => state.workspaces.raise(surface),
    }
}

/// A finger has landed. Find what it landed on and tell that client.
///
/// The surface is settled here and remembered for the life of the point, so
/// every later motion and the eventual lift go to the same client however far
/// the finger travels.
fn touch_down(state: &mut CompositorState, time_ms: u32, id: i32, x: f64, y: f64) {
    state.dirty = true;
    let Some(hit) = hit_test(state, x, y) else {
        // Nothing there. The point is not recorded, so its motion and lift are
        // dropped too rather than being delivered to whatever is touched next.
        return;
    };

    // Touching a window raises and focuses it, the same as clicking one.
    raise_window(state, hit.toplevel);
    switch_focus(state, hit.toplevel);

    state.touch_points.insert(id, hit.surface);
    let (local_x, local_y) = (x - f64::from(hit.surface_x), y - f64::from(hit.surface_y));
    for touch_id in wl_touch::touches_of(state, hit.surface.0) {
        wl_touch::send_down(
            state,
            hit.surface.0,
            touch_id,
            time_ms,
            hit.surface.1,
            id,
            local_x,
            local_y,
        );
        wl_touch::send_frame(state, hit.surface.0, touch_id);
    }
}

/// A finger already down has moved, in the coordinates of the surface it
/// started on — which may be nowhere near where it is now.
fn touch_motion(state: &mut CompositorState, time_ms: u32, id: i32, x: f64, y: f64) {
    state.dirty = true;
    let Some(&surface) = state.touch_points.get(&id) else {
        return;
    };
    let origin = surface_global_position(state, surface.0, surface.1);
    let (local_x, local_y) = (x - f64::from(origin.0), y - f64::from(origin.1));
    for touch_id in wl_touch::touches_of(state, surface.0) {
        wl_touch::send_motion(state, surface.0, touch_id, time_ms, id, local_x, local_y);
        wl_touch::send_frame(state, surface.0, touch_id);
    }
}

/// A finger has been lifted.
fn touch_up(state: &mut CompositorState, time_ms: u32, id: i32) {
    state.dirty = true;
    let Some(surface) = state.touch_points.remove(&id) else {
        return;
    };
    for touch_id in wl_touch::touches_of(state, surface.0) {
        wl_touch::send_up(state, surface.0, touch_id, time_ms, id);
        wl_touch::send_frame(state, surface.0, touch_id);
    }
}

/// The touch sequence has been taken over, so every point in it is void.
///
/// Every client holding a point is told, not just the one under the last
/// finger: a two-finger gesture spanning two windows leaves both of them
/// waiting to be told how it ended.
fn touch_cancel(state: &mut CompositorState) {
    state.dirty = true;
    let mut clients: Vec<u32> = state.touch_points.values().map(|s| s.0).collect();
    clients.sort_unstable();
    clients.dedup();
    state.touch_points.clear();
    for client_id in clients {
        for touch_id in wl_touch::touches_of(state, client_id) {
            wl_touch::send_cancel(state, client_id, touch_id);
        }
    }
}

/// Deliver a scroll to whichever client has the pointer.
///
/// The order within the frame is the protocol's and is not arbitrary: the
/// source first, so a client knows what kind of scroll it is about to be told
/// about; then the detent count for an axis, before the distance it explains;
/// then the distance; then the frame that says the picture is complete. A
/// client acting on the distance before it knows the source cannot decide
/// whether the scroll may have momentum.
fn deliver_scroll(
    state: &mut CompositorState,
    time_ms: u32,
    dx: f64,
    dy: f64,
    source: ScrollSource,
    v120_x: i32,
    v120_y: i32,
) {
    let Some((pointer_client, _)) = state.pointer_surface else {
        return;
    };
    // A touchpad scroll ends, and the axes it was moving are the ones that have
    // to be told so. A wheel never stops in this sense — it is between detents,
    // not finished.
    if source == ScrollSource::Finger {
        state.scrolling_vertical |= dy != 0.0;
        state.scrolling_horizontal |= dx != 0.0;
    }

    for ptr in state.pointers.clone() {
        if ptr.client_id != pointer_client {
            continue;
        }
        wl_pointer::send_axis_source(state, ptr.client_id, ptr.object_id, source);
        if dy != 0.0 {
            wl_pointer::send_axis_steps(
                state,
                ptr.client_id,
                ptr.object_id,
                wl_pointer::AXIS_VERTICAL,
                v120_y,
            );
            wl_pointer::send_axis(
                state,
                ptr.client_id,
                ptr.object_id,
                time_ms,
                wl_pointer::AXIS_VERTICAL,
                dy * SCROLL_STEP,
            );
        }
        if dx != 0.0 {
            wl_pointer::send_axis_steps(
                state,
                ptr.client_id,
                ptr.object_id,
                wl_pointer::AXIS_HORIZONTAL,
                v120_x,
            );
            wl_pointer::send_axis(
                state,
                ptr.client_id,
                ptr.object_id,
                time_ms,
                wl_pointer::AXIS_HORIZONTAL,
                dx * SCROLL_STEP,
            );
        }
        wl_pointer::send_frame(state, ptr.client_id, ptr.object_id);
    }
}

/// Tell the client a touchpad scroll has finished.
///
/// Only the axes that were actually moving are stopped: an `axis_stop` for an
/// axis that never scrolled is a statement about something that never happened.
fn deliver_scroll_end(state: &mut CompositorState, time_ms: u32) {
    let (vertical, horizontal) = (state.scrolling_vertical, state.scrolling_horizontal);
    state.scrolling_vertical = false;
    state.scrolling_horizontal = false;
    if !vertical && !horizontal {
        return;
    }
    let Some((pointer_client, _)) = state.pointer_surface else {
        return;
    };

    for ptr in state.pointers.clone() {
        if ptr.client_id != pointer_client {
            continue;
        }
        wl_pointer::send_axis_source(state, ptr.client_id, ptr.object_id, ScrollSource::Finger);
        if vertical {
            wl_pointer::send_axis_stop(
                state,
                ptr.client_id,
                ptr.object_id,
                time_ms,
                wl_pointer::AXIS_VERTICAL,
            );
        }
        if horizontal {
            wl_pointer::send_axis_stop(
                state,
                ptr.client_id,
                ptr.object_id,
                time_ms,
                wl_pointer::AXIS_HORIZONTAL,
            );
        }
        wl_pointer::send_frame(state, ptr.client_id, ptr.object_id);
    }
}

/// Whether either Alt is held, for the compositor's own drag bindings.
fn alt_held(state: &CompositorState) -> bool {
    state.pressed_keys.contains(&KEY_LEFTALT) || state.pressed_keys.contains(&KEY_RIGHTALT)
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

/// Tell clients which outputs each of their surfaces is displayed on.
///
/// A client cannot choose a buffer scale without this: `wl_output.scale` is per
/// output, so the surface has to know which outputs it overlaps. The full set is
/// recomputed and diffed against what each surface has already been told, so
/// this is idempotent and safe to run every frame.
fn update_surface_outputs(state: &mut CompositorState) {
    let mut enters: Vec<(u32, u32, OutputId)> = Vec::new();
    let mut leaves: Vec<(u32, u32, OutputId)> = Vec::new();
    // What is actually on screen, which is not what the client has been told:
    // an output it has not bound is one it cannot be told about. Collected
    // here and stored below, since the loop only has the surfaces borrowed.
    let mut visible: Vec<((u32, u32), HashSet<OutputId>)> = Vec::new();

    for (&(client_id, surface_id), surface) in &state.surfaces {
        // An unmapped surface is on no output, and neither is one belonging to
        // a window on a workspace that is not showing.
        if surface.buffer_id.is_none() || !is_visible(state, (client_id, surface_id)) {
            leaves.extend(
                surface
                    .entered_outputs
                    .iter()
                    .map(|&output_id| (client_id, surface_id, output_id)),
            );
            visible.push(((client_id, surface_id), HashSet::new()));
            continue;
        }

        let (x, y) = surface_global_position(state, client_id, surface_id);
        let (w, h) = surface_dimensions(state, (client_id, surface_id));

        let mut on = HashSet::new();
        for output in &state.outputs {
            let geometry = &output.geometry;
            let overlaps = x < geometry.x + geometry.physical_width
                && x.saturating_add(w) > geometry.x
                && y < geometry.y + geometry.physical_height
                && y.saturating_add(h) > geometry.y;
            if overlaps {
                on.insert(output.id);
            }
            match (overlaps, surface.entered_outputs.contains(&output.id)) {
                (true, false) => enters.push((client_id, surface_id, output.id)),
                (false, true) => leaves.push((client_id, surface_id, output.id)),
                _ => {}
            }
        }
        visible.push(((client_id, surface_id), on));
    }

    for (key, on) in visible {
        if let Some(surface) = state.surfaces.get_mut(&key) {
            surface.visible_on = on;
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

/// Format names and modifier counts, for a log line.
fn describe_formats<'a>(formats: impl Iterator<Item = &'a crate::shared::DmabufFormat>) -> String {
    formats
        .map(|f| {
            format!(
                "{} ({} modifier(s))",
                fourcc_name(f.fourcc),
                f.modifiers.len()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a surface is part of a window the user can currently see.
///
/// Popups and subsurfaces have no workspace of their own, so the question is
/// really about the toplevel they hang off: walking up to the root surface is
/// what turns one into the other. A surface with no toplevel above it — a
/// cursor surface, or one whose role has not been assigned yet — is in no
/// workspace and so not visible as a window.
fn is_visible(state: &CompositorState, key: ClientObjectId) -> bool {
    state.workspaces.is_visible(root_surface(state, key))
}

/// Walk up the parent chain to the surface at the root of the tree.
fn root_surface(state: &CompositorState, key: ClientObjectId) -> ClientObjectId {
    let (client_id, mut current) = key;
    // Bounded by the number of surfaces: the parent links form a tree, and
    // `wl_subcompositor` rejects a cycle when the link is made.
    while let Some(parent) = state
        .surfaces
        .get(&(client_id, current))
        .and_then(|s| s.parent)
    {
        current = parent;
    }
    (client_id, current)
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
/// Cycling stays within one workspace — the one holding the focused window,
/// or failing that the one showing on the output under the pointer. The other
/// workspaces are not on screen, so tabbing into them would move focus to a
/// window the user cannot see.
///
/// A workspace stack is ordered bottom to top, so taking the bottom entry and
/// pushing it on top rotates through every window on repeated presses rather
/// than toggling between the most recent two.
fn cycle_focus(state: &mut CompositorState) {
    let Some(output_id) = state
        .focused_surface
        .and_then(|key| state.surface_output(key))
        .or_else(|| state.output_for_new_window())
    else {
        return;
    };
    let Some(workspace) = state.workspaces.active_mut(output_id) else {
        return;
    };
    if workspace.surface_stack.len() < 2 {
        return;
    }
    let next = workspace.surface_stack.remove(0);
    workspace.surface_stack.push(next);
    state.dirty = true;
    switch_focus(state, next);
}

/// Fire the pending frame callbacks and presentation feedbacks of the surfaces
/// `target` covers.
///
/// `presented_at` is the moment the backend put the frame on screen, and
/// `None` says nothing was put anywhere — which is the honest answer for a
/// surface no output is showing, and makes its presentation feedback
/// `discarded` rather than a `presented` naming a time nothing happened.
fn fire_frame_callbacks(
    state: &mut CompositorState,
    timestamp_ms: u32,
    target: FrameTarget,
    presented_at: Option<PresentedAt>,
) {
    let mut callbacks: Vec<(u32, u32)> = Vec::new(); // (client_id, callback_id)
    let mut presentation: Vec<(u32, u32)> = Vec::new(); // (client_id, feedback_id)

    for surface in state.surfaces.values_mut() {
        // `visible_on` rather than `entered_outputs`: the latter is what the
        // client has been told, and a client that never bound `wl_output` has
        // been told nothing. Pacing it on that would leave it waiting on a
        // callback that could not arrive.
        let covered = match target {
            FrameTarget::Output(output_id) => surface.visible_on.contains(&output_id),
            FrameTarget::Offscreen => surface.visible_on.is_empty(),
        };
        if !covered {
            continue;
        }
        if let Some(callback_id) = surface.frame_callback.take() {
            callbacks.push((surface.client_id, callback_id));
        }
        for feedback_id in surface.presentation_feedbacks.drain(..) {
            presentation.push((surface.client_id, feedback_id));
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

    // Nothing reached a screen, so every feedback is discarded rather than
    // presented. A client hearing `presented` with a timestamp for a frame
    // that was never shown would be told a straightforward untruth, and the
    // protocol exists precisely to be accurate about this.
    let Some(presented_at) = presented_at else {
        for (client_id, feedback_id) in presentation {
            if let Some(client) = state.clients.get(client_id) {
                let _ = client.send(message(
                    feedback_id,
                    wp_presentation_feedback::DISCARDED,
                    Vec::new(),
                ));
                client.unregister(feedback_id);
            }
        }
        return;
    };

    // Fire wp_presentation_feedback.presented events
    if !presentation.is_empty() {
        // The backend's own clock reading, taken when it presented, rather than
        // one measured here a channel hop later. Accurate timing is the whole
        // point of this protocol.
        let tv_sec_hi = (presented_at.tv_sec.cast_unsigned() >> 32) as u32;
        let tv_sec_lo = u32::try_from(presented_at.tv_sec).unwrap_or(0);
        let tv_nsec = u32::try_from(presented_at.tv_nsec).unwrap_or(0);

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
                // Opcode 1. Sending this payload as opcode 0 makes the
                // client decode it as `sync_output`, whose single argument is
                // a non-nullable object — a zero there is a fatal decode error
                // that takes the connection down.
                let _ = client.send(message(
                    feedback_id,
                    wp_presentation_feedback::PRESENTED,
                    args,
                ));
                client.unregister(feedback_id);
            } else {
                debug!(
                    "Client {} disappeared before presentation feedback could be fired",
                    client_id
                );
            }
        }
    }
}

/// Note buffers a commit replaced as no longer wanted.
///
/// This is not the release itself: a frame still in flight may be reading the
/// buffer, and telling the client it may draw would corrupt what is on screen.
/// Buffers still attached to a surface are skipped, which happens when several
/// commits are batched and a buffer is re-attached after being replaced.
fn start_buffer_releases(state: &mut CompositorState) {
    for (client_id, buffer_id) in std::mem::take(&mut state.buffers_pending_release) {
        let still_attached = state
            .surfaces
            .values()
            .any(|s| s.client_id == client_id && s.buffer_id == Some(buffer_id));
        if still_attached {
            continue;
        }
        state.releasing_buffers.insert((client_id, buffer_id));
    }
}

/// Tell clients about buffers nothing is reading any more.
///
/// Runs every tick, not only the ones that draw: the last reader is usually the
/// previous frame, which is dropped when a later frame replaces it or the
/// backend finishes with it, and neither is tied to this compositor's idea of
/// whether anything changed.
fn finish_buffer_releases(state: &mut CompositorState) {
    if state.releasing_buffers.is_empty() {
        return;
    }
    let buffers_to_release: Vec<ClientObjectId> = state
        .releasing_buffers
        .iter()
        .filter(|&&key| !state.buffer_is_being_read(key))
        // A client can re-attach a buffer before hearing it was released.
        // Telling it the buffer is free while it is on screen would invite it
        // to draw over what is being displayed.
        .filter(|&&(client_id, buffer_id)| {
            !state
                .surfaces
                .values()
                .any(|s| s.client_id == client_id && s.buffer_id == Some(buffer_id))
        })
        .copied()
        .collect();
    for key in &buffers_to_release {
        state.releasing_buffers.remove(key);
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

/// The size range an interactive resize of a window may produce.
///
/// Three sources, and the narrowest wins where they overlap:
///
/// - the compositor's own floor, so the window stays grabbable;
/// - the client's `set_min_size` and `set_max_size`, which say what it can
///   actually render at;
/// - the size of the output the window is on, so a drag cannot make a window
///   larger than the display showing it.
///
/// Where they contradict each other the range is widened rather than inverted,
/// because a clamp needs a floor no higher than its ceiling. A client that
/// insists on a minimum larger than the display gets it: the compositor cannot
/// make it render smaller, and configuring a size it will refuse achieves
/// nothing.
fn resize_limits(state: &CompositorState, surface: ClientObjectId) -> ((i32, i32), (i32, i32)) {
    let (display_width, display_height) = state
        .surface_output(surface)
        .and_then(|id| state.outputs.iter().find(|o| o.id == id))
        .map_or((i32::MAX, i32::MAX), |o| {
            (o.geometry.physical_width, o.geometry.physical_height)
        });
    let ((client_min_w, client_min_h), (client_max_w, client_max_h)) =
        state.client_size_limits(surface);

    let min_width = MIN_WINDOW_WIDTH.max(client_min_w);
    let min_height = MIN_WINDOW_HEIGHT.max(client_min_h);
    // Zero means the client named no limit, so only the display applies.
    let max_width = limit_or(client_max_w, display_width).max(min_width);
    let max_height = limit_or(client_max_h, display_height).max(min_height);

    ((min_width, min_height), (max_width, max_height))
}

/// A client's stated maximum narrowed by the display's, treating zero as unset.
fn limit_or(client_limit: i32, display_limit: i32) -> i32 {
    if client_limit > 0 {
        client_limit.min(display_limit)
    } else {
        display_limit
    }
}

/// Drive the active grab from a pointer position. Returns false if there is none.
fn update_grab(state: &mut CompositorState, x: f64, y: f64) -> bool {
    let Some(grab) = state.pointer_grab else {
        return false;
    };
    match grab.kind {
        GrabKind::Move { offset_x, offset_y } => {
            let position = (f64_to_i32(x) + offset_x, f64_to_i32(y) + offset_y);
            if let Some(surface) = state.surfaces.get_mut(&grab.surface) {
                surface.position = position;
            }
            // Dragging toward another output hands the window over rather than
            // letting it straddle: the window follows the pointer's output, and
            // the tick's confinement then pulls it wholly inside that one.
            let pointer_output = state
                .outputs
                .iter()
                .find(|o| output_contains(o, f64_to_i32(x), f64_to_i32(y)))
                .map(|o| o.id);
            if let Some(output_id) = pointer_output {
                state.move_toplevel_to_output(grab.surface, output_id);
            }
        }
        GrabKind::Resize {
            edges,
            start_pointer,
            start_position,
            start_size,
            last_sent,
        } => {
            let dx = f64_to_i32(x - start_pointer.0);
            let dy = f64_to_i32(y - start_pointer.1);

            let ((min_width, min_height), (max_width, max_height)) =
                resize_limits(state, grab.surface);

            // Each edge moves independently, and the opposite edge stays put:
            // dragging the left edge changes width and origin together. The
            // origin is derived from the clamped width, so it stays pinned to
            // the anchored edge even once the drag runs out of room.
            let mut width = start_size.0;
            let mut height = start_size.1;
            let mut position = start_position;
            if edges.right() {
                width = (start_size.0 + dx).clamp(min_width, max_width);
            } else if edges.left() {
                width = (start_size.0 - dx).clamp(min_width, max_width);
                position.0 = start_position.0 + start_size.0 - width;
            }
            if edges.bottom() {
                height = (start_size.1 + dy).clamp(min_height, max_height);
            } else if edges.top() {
                height = (start_size.1 - dy).clamp(min_height, max_height);
                position.1 = start_position.1 + start_size.1 - height;
            }

            if let Some(surface) = state.surfaces.get_mut(&grab.surface) {
                surface.position = position;
            }
            // The client decides its own size; all we can do is ask. Asking on
            // every motion event would flood it, so an unchanged size is
            // silence.
            if (width, height) != last_sent {
                if let Some(g) = state.pointer_grab.as_mut()
                    && let GrabKind::Resize { last_sent, .. } = &mut g.kind
                {
                    *last_sent = (width, height);
                }
                configure_resizing(state, grab.toplevel, width, height, true);
            }
        }
    }
    state.dirty = true;
    true
}

/// End the active grab, telling a resized client it has stopped resizing.
fn end_grab(state: &mut CompositorState) {
    let Some(grab) = state.pointer_grab.take() else {
        return;
    };
    if let GrabKind::Resize { last_sent, .. } = grab.kind {
        configure_resizing(state, grab.toplevel, last_sent.0, last_sent.1, false);
    }
    state.dirty = true;
}

/// Deliver a drag to whatever is under the pointer. Returns false if there is
/// no drag, in which case the pointer belongs to the ordinary path.
///
/// The surface under the pointer hears about this through its *data device*,
/// not its pointer: no `wl_pointer` event reaches anyone for the duration of a
/// drag, and the target learns where the pointer is from
/// `wl_data_device.motion`.
fn update_drag(state: &mut CompositorState, time_ms: u32) -> bool {
    let Some(drag) = state.drag.clone() else {
        return false;
    };
    let (x, y) = (state.cursor_x, state.cursor_y);
    state.dirty = true;

    // A drag with no source is one client dragging within itself. It has
    // nothing to hand anyone else, so nobody else is a target — filtering here
    // rather than at delivery keeps the enter and leave bookkeeping honest,
    // since the drag never considers itself to have entered a surface it may
    // not talk to.
    let hit = hit_test(state, x, y)
        .filter(|h| drag.source.is_some() || h.surface.0 == drag.origin_client);
    let new_focus = hit.as_ref().map(|h| h.surface);

    if new_focus == drag.focus {
        if let Some(h) = hit {
            let (local_x, local_y) = (x - f64::from(h.surface_x), y - f64::from(h.surface_y));
            for device_id in wl_data_device::devices_of(state, h.surface.0) {
                wl_data_device::send_motion(
                    state,
                    h.surface.0,
                    device_id,
                    time_ms,
                    local_x,
                    local_y,
                );
            }
        }
        return true;
    }

    state.end_drag_focus();
    if let Some(h) = hit {
        let (local_x, local_y) = (x - f64::from(h.surface_x), y - f64::from(h.surface_y));
        enter_drag_surface(state, h.surface, local_x, local_y);
    }
    true
}

/// Introduce a drag to the client whose surface it has just moved over.
///
/// One offer per data device, because `enter` is a per-device event and an
/// offer belongs to the device it arrived on. The order within a device is
/// forced: the client has to know the object exists and what mime types are
/// behind it before it is told a drag is over it and asked to decide.
fn enter_drag_surface(state: &mut CompositorState, surface: ClientObjectId, x: f64, y: f64) {
    let Some(source) = state.drag.as_ref().map(|drag| drag.source) else {
        return;
    };
    let client_id = surface.0;
    let source_actions = source
        .and_then(|key| state.data_sources.get(&key))
        .map_or(0, |s| s.actions);

    let mut offers = Vec::new();
    for device_id in wl_data_device::devices_of(state, client_id) {
        let offer = source.and_then(|source| {
            wl_data_device::create_offer(state, client_id, device_id, source, OfferKind::Drag)
        });
        if let Some(offer) = offer {
            // Before the enter, so the client has the whole picture at the
            // moment it decides what it will accept.
            wl_data_offer::send_source_actions(state, offer, source_actions);
            offers.push(offer);
        }
        wl_data_device::send_enter(
            state,
            client_id,
            device_id,
            surface.1,
            x,
            y,
            offer.map(|(_, offer_id)| offer_id),
        );
    }

    if let Some(drag) = state.drag.as_mut() {
        drag.focus = Some(surface);
        drag.focus_offers.clone_from(&offers);
    }
    // After the enter, so the client has already been told the offer exists
    // before it hears what action it would settle on.
    for offer in offers {
        state.resolve_offer_action(offer);
    }
}

/// Resolve a drag when the button comes up.
///
/// A drop frees the pointer immediately and does *not* end the offer. The
/// target has still to read the data and say when it is done, and everything
/// that needs is reachable from the offer rather than from the drag — so the
/// drag is what ends here and the offer is what survives. Keeping the drag
/// alive to mean "dropped but not finished" would put a second condition on
/// every check of whether the pointer is spoken for, and the one place that
/// forgot it would swallow the pointer for good.
fn finish_drag(state: &mut CompositorState) {
    // Read before the drag is taken: this asks what the target accepted
    // through it.
    let accepted = state.drag_target_accepted();
    let Some(drag) = state.drag.take() else {
        return;
    };
    state.dirty = true;

    let Some(focus) = drag.focus else {
        // Let go over nothing. There is nobody to drop on, so the source is
        // told the drag came to nothing.
        if let Some(source) = drag.source {
            wl_data_source::send_cancelled(state, source);
        }
        return;
    };

    // A drag with no source went only to the client that started it, which
    // handles the content itself: there is no source to tell and no offer to
    // keep alive.
    if drag.source.is_none() {
        for device_id in wl_data_device::devices_of(state, focus.0) {
            wl_data_device::send_drop(state, focus.0, device_id);
        }
        return;
    }

    if !accepted {
        for device_id in wl_data_device::devices_of(state, focus.0) {
            wl_data_device::send_leave(state, focus.0, device_id);
        }
        for &offer in &drag.focus_offers {
            state.invalidate_offer(offer);
        }
        if let Some(source) = drag.source {
            wl_data_source::send_cancelled(state, source);
        }
        return;
    }

    for device_id in wl_data_device::devices_of(state, focus.0) {
        wl_data_device::send_drop(state, focus.0, device_id);
    }
    for &offer in &drag.focus_offers {
        state.mark_offer_dropped(offer);
    }
    if let Some(source) = drag.source {
        wl_data_source::send_dnd_drop_performed(state, source);
    }
}

/// Ask a client for a size, marking whether the resize is still in progress.
fn configure_resizing(
    state: &mut CompositorState,
    toplevel: ClientObjectId,
    width: i32,
    height: i32,
    resizing: bool,
) {
    xdg_toplevel::send_resize_configure(state, toplevel.0, toplevel.1, width, height, resizing);
    // The size only takes effect once the client acknowledges the matching
    // xdg_surface.configure, so the two always travel together.
    if let Some(xdg_surface_id) = state.xdg_toplevels.get(&toplevel).map(|t| t.xdg_surface_id) {
        let serial = crate::compositor::protocol::next_serial();
        xdg_surface::send_configure(state, toplevel.0, xdg_surface_id, serial);
    }
}

/// Which edges an Alt+drag resize should pull, from where in the window the
/// pointer sits: the nearest corner, so any part of the window is usable.
fn edges_for_point(
    state: &CompositorState,
    surface: ClientObjectId,
    x: f64,
    y: f64,
) -> ResizeEdges {
    let Some(position) = state.surfaces.get(&surface).map(|s| s.position) else {
        return ResizeEdges(ResizeEdges::BOTTOM | ResizeEdges::RIGHT);
    };
    let (width, height) = state.surface_size(surface);
    let horizontal = if f64_to_i32(x) < position.0 + width / 2 {
        ResizeEdges::LEFT
    } else {
        ResizeEdges::RIGHT
    };
    let vertical = if f64_to_i32(y) < position.1 + height / 2 {
        ResizeEdges::TOP
    } else {
        ResizeEdges::BOTTOM
    };
    ResizeEdges(horizontal | vertical)
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

    // The clipboard follows keyboard focus, so this is where a client is told
    // what is on it. After the keyboard enter, because having focus is what
    // makes the selection this client's to read.
    //
    // The client losing focus is told nothing: an offer it still holds stops
    // working the moment the selection is replaced, and it is not going to be
    // asked for a paste in the meantime.
    wl_data_device::send_selection_to_client(state, new_client);
}

/// Every window on screen, topmost first.
///
/// Only the workspace showing on each output contributes: a window on a
/// workspace that is not displayed cannot be clicked. Windows are confined to
/// their own output and never straddle two, so the order between outputs
/// cannot matter — no two entries here overlap unless they share an output.
fn visible_toplevels_top_down(state: &protocol::CompositorState) -> Vec<ClientObjectId> {
    let mut keys: Vec<ClientObjectId> = state
        .outputs
        .iter()
        .flat_map(|output| state.workspaces.visible_stack(output.id))
        .copied()
        .collect();
    keys.reverse();
    keys
}

/// Hit-test the visible windows from top to bottom. Returns the toplevel and
/// the specific surface (possibly a subsurface) under the pointer.
fn hit_test(state: &protocol::CompositorState, x: f64, y: f64) -> Option<HitResult> {
    let px = f64_to_i32(x);
    let py = f64_to_i32(y);

    for key in visible_toplevels_top_down(state) {
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
        if let Some(result) = hit_test_surface_tree(
            state,
            child_key,
            offset_x.saturating_add(cx),
            offset_y.saturating_add(cy),
            px,
            py,
        ) {
            return Some(result);
        }
    }

    // Check this surface's own bounds
    let (w, h) = surface_dimensions(state, surface_key);
    if w == 0 || h == 0 {
        return None;
    }
    if px < offset_x
        || py < offset_y
        || px >= offset_x.saturating_add(w)
        || py >= offset_y.saturating_add(h)
    {
        return None;
    }

    // Within the surface's bounds, but the client may have narrowed which parts
    // of it accept pointer input. Clients drawing their own decorations use this
    // to let clicks fall through the drop shadow around the window.
    if let Some(input_region) = &surface.input_region
        && !region_contains(
            input_region,
            px.saturating_sub(offset_x),
            py.saturating_sub(offset_y),
        )
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
    let Some(buf) = state.buffers.get(&(client_id, buffer_id)) else {
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
        x = x.saturating_add(surface.subsurface_position.0);
        y = y.saturating_add(surface.subsurface_position.1);
        if let Some(parent_id) = surface.parent {
            current = parent_id;
        } else {
            // Root surface — add its global position
            x = x.saturating_add(surface.position.0);
            y = y.saturating_add(surface.position.1);
            break;
        }
    }

    (x, y)
}
