//! Tests for the compositor event loop: key bindings, focus, hit
//! testing, buffer release, window placement, and interactive grabs.

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
    use crate::compositor::protocol::state::ShmBuffer;
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
            content_serial: 1,
            damage: None,
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

use super::scene;
use super::{finish_buffer_releases, start_buffer_releases};
use crate::shared::OutputId;

/// A pool with two buffers, and a mapped surface showing the first.
fn state_with_two_buffers() -> CompositorState {
    use std::io::Write;
    use std::os::fd::{FromRawFd, IntoRawFd};

    use crate::shared::{
        OUTPUT_MODE_CURRENT, Output, OutputGeometry, OutputMode, OutputSubpixel, OutputTransform,
    };

    let mut state = CompositorState::new();
    let _rx = add_client(&mut state, 1);
    state.outputs.push(Output {
        id: OutputId(1),
        geometry: OutputGeometry {
            x: 0,
            y: 0,
            physical_width: 64,
            physical_height: 64,
            subpixel: OutputSubpixel::None,
            make: String::new(),
            model: String::new(),
            transform: OutputTransform::Normal,
        },
        modes: vec![OutputMode {
            flags: OUTPUT_MODE_CURRENT,
            width: 64,
            height: 64,
            refresh_mhz: 60000,
        }],
        scale: 1,
        name: String::from("test"),
        description: String::from("test"),
    });

    let size = 64 * 4 * 2;
    let fd = unsafe { libc::memfd_create(c"release-test".as_ptr().cast(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(&vec![0u8; size]).unwrap();
    file.flush().unwrap();
    state.register_shm_pool(1, 100, file.into_raw_fd(), size.try_into().unwrap());

    // Two 8x8 buffers, one in each half of the pool.
    state.register_buffer(1, 101, 100, 0, 8, 8, 32, 0);
    state.register_buffer(1, 102, 100, 256, 8, 8, 32, 0);

    state.create_surface(1, 200);
    let surface = state.surfaces.get_mut(&(1, 200)).unwrap();
    surface.buffer_id = Some(101);
    surface.output = Some(OutputId(1));
    state.surface_stack.push((1, 200));
    state
}

#[test]
fn a_replaced_buffer_waits_for_its_last_reader() {
    let mut state = state_with_two_buffers();
    let mut cache = scene::SceneCache::new();

    // A frame is drawn from buffer 101, so something is reading it.
    let frame = scene::build(OutputId(1), &state, &mut cache);

    // The client swaps to 102, which retires 101.
    state.surfaces.get_mut(&(1, 200)).unwrap().buffer_id = Some(102);
    state.buffers_pending_release.push((1, 101));
    start_buffer_releases(&mut state);
    assert!(state.releasing_buffers.contains(&(1, 101)));

    // The frame still borrows it, so the client must not be told yet.
    finish_buffer_releases(&mut state);
    assert!(state.releasing_buffers.contains(&(1, 101)));

    drop(frame);
    finish_buffer_releases(&mut state);
    assert!(state.releasing_buffers.is_empty());
}

#[test]
fn a_buffer_still_on_screen_is_not_released() {
    let mut state = state_with_two_buffers();

    // Retired and then re-attached before the release went out.
    state.buffers_pending_release.push((1, 101));
    start_buffer_releases(&mut state);
    assert!(state.releasing_buffers.is_empty(), "still attached");

    // Detached, so it may now be retired.
    state.surfaces.get_mut(&(1, 200)).unwrap().buffer_id = None;
    state.buffers_pending_release.push((1, 101));
    start_buffer_releases(&mut state);
    finish_buffer_releases(&mut state);
    assert!(state.releasing_buffers.is_empty());
}

/// Two outputs side by side, each 100 wide.
fn state_with_two_outputs() -> CompositorState {
    state_with_two_outputs_sized(100, 100)
}

/// Two outputs side by side of the given size.
fn state_with_two_outputs_sized(width: i32, height: i32) -> CompositorState {
    use crate::shared::{
        OUTPUT_MODE_CURRENT, Output, OutputGeometry, OutputMode, OutputSubpixel, OutputTransform,
    };
    let mut state = CompositorState::new();
    for (index, x) in [(1u32, 0i32), (2, width)] {
        state.outputs.push(Output {
            id: OutputId(index),
            geometry: OutputGeometry {
                x,
                y: 0,
                physical_width: width,
                physical_height: height,
                subpixel: OutputSubpixel::None,
                make: String::new(),
                model: String::new(),
                transform: OutputTransform::Normal,
            },
            modes: vec![OutputMode {
                flags: OUTPUT_MODE_CURRENT,
                width,
                height,
                refresh_mhz: 60000,
            }],
            scale: 1,
            name: String::new(),
            description: String::new(),
        });
    }
    state
}

#[test]
fn a_new_window_opens_on_the_output_under_the_pointer() {
    let mut state = state_with_two_outputs();
    let _rx = add_client(&mut state, 1);
    // Pointer on the second output.
    state.cursor_x = 150.0;
    state.cursor_y = 50.0;

    add_toplevel(&mut state, 1, 10, 11, 12);

    let surface = &state.surfaces[&(1, 10)];
    assert_eq!(surface.output, Some(OutputId(2)));
    // Placed within that output, not at the global origin.
    assert!(
        surface.position.0 >= 100 && surface.position.0 < 200,
        "placed at {:?}, outside its own output",
        surface.position
    );
}

#[test]
fn each_output_cascades_windows_independently() {
    let mut state = state_with_two_outputs();
    let _rx = add_client(&mut state, 1);

    state.cursor_x = 50.0;
    add_toplevel(&mut state, 1, 10, 11, 12);
    state.cursor_x = 150.0;
    add_toplevel(&mut state, 1, 20, 21, 22);

    // First window on each output gets that output's first cascade slot,
    // so the two sit at the same place relative to their own output.
    let first = state.surfaces[&(1, 10)].position;
    let second = state.surfaces[&(1, 20)].position;
    assert_eq!(second.0 - 100, first.0);
    assert_eq!(second.1, first.1);
}

#[test]
fn the_cascade_restarts_rather_than_marching_off_the_output() {
    let mut state = state_with_two_outputs();
    let _rx = add_client(&mut state, 1);
    state.cursor_x = 50.0;

    // Enough windows that a bounded cascade must wrap.
    for i in 0..12u32 {
        add_toplevel(&mut state, 1, 100 + i * 3, 101 + i * 3, 102 + i * 3);
    }

    for key in &state.surface_stack {
        let position = state.surfaces[key].position;
        assert!(
            position.0 < 100 && position.1 < 100,
            "window placed at {position:?}, off its own output"
        );
    }
}

use super::{
    MIN_WINDOW_HEIGHT, MIN_WINDOW_WIDTH, edges_for_point, end_grab, resize_limits, update_grab,
};
use crate::compositor::protocol::state::{ClientObjectId, GrabKind, ResizeEdges, ShmBuffer};

const WINDOW: ClientObjectId = (1, 10);
const WINDOW_W: i32 = 200;
const WINDOW_H: i32 = 150;

const OUTPUT_W: i32 = 1000;
const OUTPUT_H: i32 = 800;

/// A mapped window at (10, 10), 200x150, on the first of two large
/// outputs — large enough that a resize has room before it hits the
/// display-size ceiling.
fn state_with_grabbable_window() -> CompositorState {
    let mut state = state_with_two_outputs_sized(OUTPUT_W, OUTPUT_H);
    let _rx = add_client(&mut state, 1);
    add_toplevel(&mut state, 1, 10, 11, 12);
    state.shm_buffers.insert(
        (1, 13),
        ShmBuffer {
            client_id: 1,
            pool_id: 0,
            offset: 0,
            width: WINDOW_W,
            height: WINDOW_H,
            stride: WINDOW_W * 4,
            format: 0,
            content_serial: 1,
            damage: None,
        },
    );
    let surface = state.surfaces.get_mut(&WINDOW).unwrap();
    surface.buffer_id = Some(13);
    surface.position = (10, 10);
    surface.output = Some(OutputId(1));
    state
}

fn resize_size(state: &CompositorState) -> (i32, i32) {
    match state.pointer_grab.expect("no grab").kind {
        GrabKind::Resize { last_sent, .. } => last_sent,
        GrabKind::Move { .. } => panic!("expected a resize grab"),
    }
}

#[test]
fn a_move_grab_carries_the_window_with_the_pointer() {
    let mut state = state_with_grabbable_window();
    state.cursor_x = 20.0;
    state.cursor_y = 20.0;
    state.start_move_grab(WINDOW);

    assert!(update_grab(&mut state, 45.0, 35.0));
    // The grip point stays under the cursor, so the window moves by the
    // same delta the pointer did.
    assert_eq!(state.surfaces[&WINDOW].position, (35, 25));
}

#[test]
fn dragging_a_window_onto_another_output_hands_it_over() {
    let mut state = state_with_grabbable_window();
    state.cursor_x = 20.0;
    state.cursor_y = 20.0;
    state.start_move_grab(WINDOW);

    // Pointer crosses onto the second output, so the window goes with it
    // rather than straddling the two.
    update_grab(&mut state, f64::from(OUTPUT_W) + 50.0, 50.0);
    assert_eq!(state.surfaces[&WINDOW].output, Some(OutputId(2)));
}

#[test]
fn a_resize_from_the_right_grows_without_moving_the_window() {
    let mut state = state_with_grabbable_window();
    state.cursor_x = 210.0;
    state.cursor_y = 80.0;
    state.start_resize_grab(WINDOW, ResizeEdges(ResizeEdges::RIGHT));

    update_grab(&mut state, 260.0, 80.0);
    assert_eq!(resize_size(&state), (WINDOW_W + 50, WINDOW_H));
    // The opposite edge is the anchor, so the origin does not move.
    assert_eq!(state.surfaces[&WINDOW].position, (10, 10));
}

#[test]
fn a_resize_from_the_left_moves_the_origin_as_the_window_shrinks() {
    let mut state = state_with_grabbable_window();
    state.cursor_x = 10.0;
    state.cursor_y = 80.0;
    state.start_resize_grab(WINDOW, ResizeEdges(ResizeEdges::LEFT));

    update_grab(&mut state, 30.0, 80.0);
    assert_eq!(resize_size(&state), (WINDOW_W - 20, WINDOW_H));
    // The right edge stayed put, so the left edge is where the pointer is.
    assert_eq!(state.surfaces[&WINDOW].position, (30, 10));
}

#[test]
fn a_resize_will_not_shrink_a_window_to_nothing() {
    let mut state = state_with_grabbable_window();
    state.cursor_x = 210.0;
    state.cursor_y = 160.0;
    state.start_resize_grab(
        WINDOW,
        ResizeEdges(ResizeEdges::RIGHT | ResizeEdges::BOTTOM),
    );

    // Dragging far past the opposite corner. A window with no edge left to
    // grab could never be recovered.
    update_grab(&mut state, -5000.0, -5000.0);
    assert_eq!(resize_size(&state), (MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT));
}

#[test]
fn a_resize_will_not_grow_a_window_past_its_display() {
    let mut state = state_with_grabbable_window();
    state.cursor_x = 210.0;
    state.cursor_y = 160.0;
    state.start_resize_grab(
        WINDOW,
        ResizeEdges(ResizeEdges::RIGHT | ResizeEdges::BOTTOM),
    );

    update_grab(&mut state, 99_000.0, 99_000.0);
    assert_eq!(resize_size(&state), (OUTPUT_W, OUTPUT_H));
}

#[test]
fn a_clamped_resize_keeps_the_anchored_edge_where_it_was() {
    let mut state = state_with_grabbable_window();
    state.cursor_x = 10.0;
    state.cursor_y = 80.0;
    state.start_resize_grab(WINDOW, ResizeEdges(ResizeEdges::LEFT));

    // Dragged well past the right edge: the width floors, and the origin
    // follows the floored width rather than the pointer, so the right edge
    // stays exactly where it started.
    update_grab(&mut state, 9000.0, 80.0);
    let (width, _) = resize_size(&state);
    assert_eq!(width, MIN_WINDOW_WIDTH);
    let position = state.surfaces[&WINDOW].position;
    assert_eq!(position.0 + width, 10 + WINDOW_W);
}

#[test]
fn a_window_larger_than_its_display_is_brought_within_it_by_a_resize() {
    let mut state = state_with_grabbable_window();
    // A client can make itself any size; a drag brings it back in bounds.
    state.shm_buffers.get_mut(&(1, 13)).unwrap().width = OUTPUT_W * 2;
    state.cursor_x = 10.0 + f64::from(OUTPUT_W) * 2.0;
    state.cursor_y = 80.0;
    state.start_resize_grab(WINDOW, ResizeEdges(ResizeEdges::RIGHT));

    let nudged = state.cursor_x + 1.0;
    update_grab(&mut state, nudged, 80.0);
    assert_eq!(resize_size(&state).0, OUTPUT_W);
}

#[test]
fn a_grab_takes_the_pointer_away_from_the_client() {
    let mut state = state_with_grabbable_window();
    state.pointer_surface = Some(WINDOW);
    state.start_move_grab(WINDOW);
    // The client is told the pointer left; it will not hear about the drag
    // and should not be left drawing a hover state.
    assert_eq!(state.pointer_surface, None);
}

#[test]
fn ending_a_grab_releases_the_window() {
    let mut state = state_with_grabbable_window();
    state.start_move_grab(WINDOW);
    assert!(state.pointer_grab.is_some());
    end_grab(&mut state);
    assert!(state.pointer_grab.is_none());
}

#[test]
fn alt_drag_resizes_from_the_nearest_corner() {
    let state = state_with_grabbable_window();
    // Window spans (10,10)..(210,160); its centre is (110, 85).
    let top_left = edges_for_point(&state, WINDOW, 20.0, 20.0);
    assert!(top_left.left() && top_left.top());

    let bottom_right = edges_for_point(&state, WINDOW, 200.0, 150.0);
    assert!(bottom_right.right() && bottom_right.bottom());
}

/// Set the window's declared size range, as `set_min_size`/`set_max_size` do.
fn set_client_limits(state: &mut CompositorState, min: (i32, i32), max: (i32, i32)) {
    let toplevel = state.xdg_toplevels.get_mut(&(1, 12)).unwrap();
    toplevel.min_size = min;
    toplevel.max_size = max;
}

/// Drag the bottom-right corner as far as it will go in the given direction.
fn drag_corner_to(state: &mut CompositorState, x: f64, y: f64) -> (i32, i32) {
    state.cursor_x = 210.0;
    state.cursor_y = 160.0;
    state.start_resize_grab(
        WINDOW,
        ResizeEdges(ResizeEdges::RIGHT | ResizeEdges::BOTTOM),
    );
    update_grab(state, x, y);
    resize_size(state)
}

#[test]
fn a_clients_maximum_is_honoured_below_the_display_size() {
    let mut state = state_with_grabbable_window();
    set_client_limits(&mut state, (0, 0), (400, 300));
    // The client asked for less than the display allows, so it wins.
    assert_eq!(drag_corner_to(&mut state, 99_000.0, 99_000.0), (400, 300));
}

#[test]
fn the_display_still_caps_a_client_that_asks_for_more() {
    let mut state = state_with_grabbable_window();
    set_client_limits(&mut state, (0, 0), (OUTPUT_W * 4, OUTPUT_H * 4));
    // Narrowest wins, and here that is the display.
    assert_eq!(
        drag_corner_to(&mut state, 99_000.0, 99_000.0),
        (OUTPUT_W, OUTPUT_H)
    );
}

#[test]
fn a_clients_minimum_is_honoured_above_the_compositors() {
    let mut state = state_with_grabbable_window();
    set_client_limits(&mut state, (300, 250), (0, 0));
    // The client cannot render smaller than this, whatever the compositor
    // would otherwise allow.
    assert_eq!(drag_corner_to(&mut state, -9000.0, -9000.0), (300, 250));
}

#[test]
fn the_compositor_floor_still_applies_below_a_clients_minimum() {
    let mut state = state_with_grabbable_window();
    set_client_limits(&mut state, (10, 10), (0, 0));
    // A client willing to go tiny still has to stay grabbable.
    assert_eq!(
        drag_corner_to(&mut state, -9000.0, -9000.0),
        (MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT)
    );
}

#[test]
fn a_fixed_size_window_cannot_be_resized() {
    let mut state = state_with_grabbable_window();
    set_client_limits(&mut state, (WINDOW_W, WINDOW_H), (WINDOW_W, WINDOW_H));
    // Equal bounds mean the client is telling us it has exactly one size.
    assert_eq!(
        drag_corner_to(&mut state, 9000.0, 9000.0),
        (WINDOW_W, WINDOW_H)
    );
}

#[test]
fn a_client_minimum_larger_than_the_display_wins() {
    let mut state = state_with_grabbable_window();
    set_client_limits(&mut state, (OUTPUT_W * 2, OUTPUT_H * 2), (0, 0));
    // The two limits contradict. Configuring a size the client will refuse
    // achieves nothing, so the range widens rather than inverting — and it
    // must never invert, or the clamp itself would be nonsense.
    let ((min_w, min_h), (max_w, max_h)) = resize_limits(&state, WINDOW);
    assert!(min_w <= max_w && min_h <= max_h);
    assert_eq!(
        drag_corner_to(&mut state, -9000.0, -9000.0),
        (OUTPUT_W * 2, OUTPUT_H * 2)
    );
}
