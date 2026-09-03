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
    use crate::compositor::protocol::state::{Buffer, BufferKind, ShmBuffer};
    let mut state = CompositorState::new();
    let _rx = add_client(&mut state, 1);
    state.create_surface(1, 10);
    state.buffers.insert(
        (1, 11),
        Buffer {
            client_id: 1,
            width: 200,
            height: 100,
            content_serial: 1,
            kind: BufferKind::Shm(ShmBuffer {
                pool_id: 0,
                offset: 0,
                stride: 800,
                format: 0,
                damage: None,
            }),
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

/// The windows showing on the first output, bottom to top.
fn stack(state: &CompositorState) -> Vec<ClientObjectId> {
    state.workspaces.visible_stack(OutputId(1)).to_vec()
}

#[test]
fn cycle_rotates_through_every_window() {
    // Cycling works within one workspace, so the windows need an output to
    // open on rather than being held aside as unplaced.
    let mut state = state_with_two_outputs();
    let _rx = add_client(&mut state, 1);
    add_toplevel(&mut state, 1, 10, 11, 12);
    add_toplevel(&mut state, 1, 20, 21, 22);
    add_toplevel(&mut state, 1, 30, 31, 32);
    // create_xdg_toplevel pushes each new window on top, bottom-to-top.
    assert_eq!(stack(&state), vec![(1, 10), (1, 20), (1, 30)]);

    cycle_focus(&mut state);
    assert_eq!(state.focused_surface, Some((1, 10)));
    assert_eq!(stack(&state), vec![(1, 20), (1, 30), (1, 10)]);

    cycle_focus(&mut state);
    assert_eq!(state.focused_surface, Some((1, 20)));

    cycle_focus(&mut state);
    assert_eq!(state.focused_surface, Some((1, 30)));
}

#[test]
fn cycle_with_one_window_does_nothing() {
    let mut state = state_with_two_outputs();
    let _rx = add_client(&mut state, 1);
    add_toplevel(&mut state, 1, 10, 11, 12);
    state.focused_surface = Some((1, 10));

    cycle_focus(&mut state);

    assert_eq!(stack(&state), vec![(1, 10)]);
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
    state.surfaces.get_mut(&(1, 200)).unwrap().buffer_id = Some(101);
    // A window is only drawn as part of the workspace showing on its output.
    state.sync_workspaces();
    state.move_toplevel_to_output((1, 200), OutputId(1));
    state
}

#[test]
fn a_replaced_buffer_waits_for_its_last_reader() {
    let mut state = state_with_two_buffers();
    let mut cache = scene::SceneCache::new();

    // A frame is drawn from buffer 101, so something is reading it.
    let frame = scene::build(OutputId(1), 1, &state, &mut cache);

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
    // An output starts life with one workspace, which is where its windows go.
    state.sync_workspaces();
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
    assert_eq!(state.surface_output((1, 10)), Some(OutputId(2)));
    // Placed within that output, not at the global origin.
    assert!(
        surface.position.0 >= 100 && surface.position.0 < 200,
        "placed at {:?}, outside its own output",
        surface.position
    );
}

#[test]
fn a_window_mapped_before_any_output_waits_for_one() {
    let mut state = CompositorState::new();
    let _rx = add_client(&mut state, 1);

    // No output means no workspace, so there is nowhere to put it yet.
    add_toplevel(&mut state, 1, 10, 11, 12);
    assert_eq!(state.surface_output((1, 10)), None);
    assert!(state.workspaces.iter().next().is_none());

    // An output turns up, bringing a workspace with it, and the window is
    // adopted on the next tick's confinement pass.
    let outputs = state_with_two_outputs();
    state.outputs = outputs.outputs;
    assert!(state.confine_toplevels());

    assert_eq!(state.surface_output((1, 10)), Some(OutputId(1)));
    assert_eq!(state.workspaces.visible_stack(OutputId(1)), [(1, 10)]);
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

    for key in &stack(&state) {
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
use crate::compositor::protocol::state::{
    Buffer, BufferKind, ClientObjectId, GrabKind, ResizeEdges, ShmBuffer,
};

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
    state.buffers.insert(
        (1, 13),
        Buffer {
            client_id: 1,
            width: WINDOW_W,
            height: WINDOW_H,
            content_serial: 1,
            kind: BufferKind::Shm(ShmBuffer {
                pool_id: 0,
                offset: 0,
                stride: WINDOW_W * 4,
                format: 0,
                damage: None,
            }),
        },
    );
    let surface = state.surfaces.get_mut(&WINDOW).unwrap();
    surface.buffer_id = Some(13);
    surface.position = (10, 10);
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
    assert_eq!(state.surface_output(WINDOW), Some(OutputId(2)));
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
    state.buffers.get_mut(&(1, 13)).unwrap().width = OUTPUT_W * 2;
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

use crate::shared::{DRM_FORMAT_XRGB8888, DmabufImage, DmabufPlane};
use std::os::fd::OwnedFd;

/// A dma-buf description over a real descriptor.
///
/// Nothing compositor-side imports it — that needs the GL context only a
/// backend has — so a `memfd` is as good as a buffer from a driver for
/// everything the compositor does with one.
fn test_dmabuf(width: i32, height: i32) -> Arc<DmabufImage> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::memfd_create(c"dmabuf-test".as_ptr().cast(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    Arc::new(DmabufImage {
        width,
        height,
        fourcc: DRM_FORMAT_XRGB8888,
        modifier: crate::shared::DRM_FORMAT_MOD_INVALID,
        planes: vec![DmabufPlane {
            fd: Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }),
            offset: 0,
            stride: width.unsigned_abs() * 4,
        }],
    })
}

/// Put a dma-buf backed `wl_buffer` in the registry, as the protocol layer
/// will once a client can send one.
///
/// Takes the image by value because the count of live `Arc`s is exactly what
/// decides when the buffer may be released: a test holding one of its own
/// would look like a frame still drawing it.
fn add_dmabuf_buffer(state: &mut CompositorState, key: ClientObjectId, image: Arc<DmabufImage>) {
    state.buffers.insert(
        key,
        Buffer {
            client_id: key.0,
            width: image.width,
            height: image.height,
            content_serial: 1,
            kind: BufferKind::Dmabuf(image),
        },
    );
}

/// Borrow a registered dma-buf the way the scene does when it builds a texture.
fn borrow_dmabuf(state: &CompositorState, key: ClientObjectId) -> Arc<DmabufImage> {
    match &state.buffers[&key].kind {
        BufferKind::Dmabuf(image) => image.clone(),
        _ => panic!("expected a dma-buf buffer"),
    }
}

#[test]
fn a_dmabuf_is_not_released_while_something_is_reading_it() {
    let mut state = CompositorState::new();
    let _rx = add_client(&mut state, 1);
    add_dmabuf_buffer(&mut state, (1, 200), test_dmabuf(8, 8));

    // Idle: state holds the only handle on it.
    assert!(!state.buffer_is_being_read((1, 200)));

    // A frame borrows it, exactly as the scene does when it builds a texture.
    let borrowed = borrow_dmabuf(&state, (1, 200));
    assert!(
        state.buffer_is_being_read((1, 200)),
        "a dma-buf has no mapping guard, so answering from `buffer_guards` \
         alone would call it idle and release it out from under the frame"
    );

    drop(borrowed);
    assert!(!state.buffer_is_being_read((1, 200)));
}

#[test]
fn drawing_into_a_dmabuf_does_not_invalidate_the_import() {
    let mut state = CompositorState::new();
    let _rx = add_client(&mut state, 1);
    add_dmabuf_buffer(&mut state, (1, 200), test_dmabuf(8, 8));
    let serial = state.buffers[&(1, 200)].content_serial;

    // What a commit does. For an shm buffer this is the signal to re-upload;
    // for a dma-buf the backend is already sampling the memory the client drew
    // into, and a new serial would only make it throw away a good import.
    state.mark_buffer_damaged(1, 200, &[]);

    assert_eq!(state.buffers[&(1, 200)].content_serial, serial);
}

#[test]
fn a_dmabuf_surface_has_a_size_like_any_other() {
    let mut state = CompositorState::new();
    let _rx = add_client(&mut state, 1);
    add_dmabuf_buffer(&mut state, (1, 200), test_dmabuf(64, 32));
    state.create_surface(1, 10);
    state.surfaces.get_mut(&(1, 10)).unwrap().buffer_id = Some(200);

    // Layout asks a buffer how big it is and nothing else, so both kinds must
    // answer — a dma-buf window that reported no size would be invisible to
    // hit-testing and confinement as well as to the scene.
    assert_eq!(state.surface_size((1, 10)), (64, 32));
    assert_eq!(super::surface_dimensions(&state, (1, 10)), (64, 32));
}

#[test]
fn an_output_is_composed_only_once_it_has_asked() {
    let mut state = state_with_two_outputs();
    let (frames, mut rx) = tokio::sync::watch::channel(crate::shared::Frame::new());
    let mut pacer = super::FramePacer::new();

    // Something changed, but no display has said it can show anything.
    // Composing now would be work thrown away, and would have the compositor
    // guessing at a rate no display told it.
    state.dirty = true;
    pacer.publish(&mut state, &frames);
    assert!(!rx.has_changed().unwrap(), "nothing was asked for");

    pacer.request(OutputId(1));
    pacer.publish(&mut state, &frames);

    let frame = rx.borrow_and_update();
    assert_eq!(frame.len(), 1);
    assert_eq!(frame[0].output_id, OutputId(1));
}

#[test]
fn a_standing_request_is_served_the_moment_something_changes() {
    let mut state = state_with_two_outputs();
    let (frames, rx) = tokio::sync::watch::channel(crate::shared::Frame::new());
    let mut pacer = super::FramePacer::new();

    // The display is ready and nothing has changed, so there is nothing to
    // send it. The request has to outlive this moment: dropping it would make
    // the next change wait for the display to ask again.
    state.dirty = false;
    pacer.request(OutputId(1));
    pacer.publish(&mut state, &frames);
    assert!(!rx.has_changed().unwrap());

    state.dirty = true;
    pacer.publish(&mut state, &frames);
    assert!(rx.has_changed().unwrap());
}

#[test]
fn serving_one_output_keeps_the_other_ones_scene() {
    let mut state = state_with_two_outputs();
    let (frames, mut rx) = tokio::sync::watch::channel(crate::shared::Frame::new());
    let mut pacer = super::FramePacer::new();

    state.dirty = true;
    pacer.request(OutputId(1));
    pacer.request(OutputId(2));
    pacer.publish(&mut state, &frames);
    let first: Vec<u64> = rx.borrow_and_update().iter().map(|s| s.serial).collect();
    assert_eq!(first.len(), 2);

    // Only the faster display comes back for another. The slot holds one
    // value, so a frame carrying just this output would drop a scene the
    // backend had not drawn yet.
    state.dirty = true;
    pacer.request(OutputId(1));
    pacer.publish(&mut state, &frames);

    let frame = rx.borrow_and_update();
    assert_eq!(frame.len(), 2, "both outputs must still be in the frame");
    let recomposed = frame
        .iter()
        .filter(|s| !first.contains(&s.serial))
        .collect::<Vec<_>>();
    assert_eq!(recomposed.len(), 1, "only the output that asked");
    assert_eq!(recomposed[0].output_id, OutputId(1));
}

#[test]
fn an_output_that_goes_away_is_forgotten() {
    let mut state = state_with_two_outputs();
    let (frames, _rx) = tokio::sync::watch::channel(crate::shared::Frame::new());
    let mut pacer = super::FramePacer::new();

    state.dirty = true;
    pacer.request(OutputId(1));
    pacer.request(OutputId(2));
    pacer.publish(&mut state, &frames);

    state.outputs.retain(|o| o.id == OutputId(1));
    pacer.forget_gone_outputs(&state);

    // Its scene would otherwise be republished with every frame for the rest
    // of the session, holding on to the client buffers it references.
    assert_eq!(pacer.published.len(), 1);
    assert!(pacer.published.contains_key(&OutputId(1)));
}

// -- Drag and drop -----------------------------------------------------------

use super::{enter_drag_surface, finish_drag, update_drag};
use crate::compositor::protocol::state::{
    DataDeviceBinding, DataSource, DataSourceRole, OfferKind,
};
use crate::compositor::protocol::{wl_data_device, wl_pointer};

const DRAG_SOURCE: u32 = 40;
const DRAG_DEVICE_A: u32 = 41;
const DRAG_DEVICE_B: u32 = 42;

/// Give a client a data device, without going through the manager.
fn add_data_device(state: &mut CompositorState, client_id: u32, device_id: u32) {
    state
        .clients
        .get(client_id)
        .unwrap()
        .register_with_version(
            device_id,
            crate::compositor::protocol::ObjectType::WlDataDevice,
            3,
        )
        .unwrap();
    state.data_devices.push(DataDeviceBinding {
        client_id,
        object_id: device_id,
    });
}

/// A grabbable window belonging to client 1, a second client with a window of
/// its own, and a source client 1 is about to drag.
fn state_ready_to_drag() -> (
    CompositorState,
    Receiver<WaylandProtocolMessage>,
    Receiver<WaylandProtocolMessage>,
) {
    let mut state = state_with_grabbable_window();
    // `state_with_grabbable_window` made client 1 but dropped its receiver, so
    // it is remade here to watch what the origin is told.
    let (tx, origin_rx) = channel(64);
    state.clients.get(1).unwrap().sender = tx;
    add_data_device(&mut state, 1, DRAG_DEVICE_A);
    state.data_sources.insert(
        (1, DRAG_SOURCE),
        DataSource {
            mime_types: vec!["text/plain".to_string()],
            actions: crate::compositor::protocol::wl_data_device_manager::DND_ACTION_COPY,
            role: DataSourceRole::Drag,
        },
    );
    state
        .clients
        .get(1)
        .unwrap()
        .register_with_version(
            DRAG_SOURCE,
            crate::compositor::protocol::ObjectType::WlDataSource,
            3,
        )
        .unwrap();

    let target_rx = add_client(&mut state, 2);
    add_toplevel(&mut state, 2, 20, 21, 22);
    add_data_device(&mut state, 2, DRAG_DEVICE_B);
    state.buffers.insert(
        (2, 23),
        Buffer {
            client_id: 2,
            width: WINDOW_W,
            height: WINDOW_H,
            content_serial: 1,
            kind: BufferKind::Shm(ShmBuffer {
                pool_id: 0,
                offset: 0,
                stride: WINDOW_W * 4,
                format: 0,
                damage: None,
            }),
        },
    );
    let target = state.surfaces.get_mut(&(2, 20)).unwrap();
    target.buffer_id = Some(23);
    target.position = (400, 10);

    (state, origin_rx, target_rx)
}

fn sent_ops(rx: &mut Receiver<WaylandProtocolMessage>) -> Vec<(u32, u16)> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|m| (m.object_id, m.op_code))
        .collect()
}

#[test]
fn a_drag_takes_the_pointer_from_whoever_had_it() {
    let (mut state, mut origin_rx, _target_rx) = state_ready_to_drag();
    // Client 1's own window has the pointer.
    state.pointer_surface = Some(WINDOW);
    state
        .pointers
        .push(crate::compositor::protocol::state::PointerBinding {
            client_id: 1,
            object_id: 30,
        });
    drop(sent_ops(&mut origin_rx));

    state.start_drag(Some((1, DRAG_SOURCE)), 1, WINDOW, None);

    assert!(state.pointer_surface.is_none());
    assert!(
        sent_ops(&mut origin_rx).contains(&(30, wl_pointer::LEAVE)),
        "the client is told the pointer has left, so it stops drawing a hover state"
    );
}

#[test]
fn dragging_over_a_window_enters_it_and_leaving_it_leaves() {
    let (mut state, _origin_rx, mut target_rx) = state_ready_to_drag();
    state.start_drag(Some((1, DRAG_SOURCE)), 1, WINDOW, None);
    drop(sent_ops(&mut target_rx));

    // Over client 2's window.
    state.cursor_x = 450.0;
    state.cursor_y = 50.0;
    assert!(update_drag(&mut state, 0));

    let ops = sent_ops(&mut target_rx);
    assert!(
        ops.contains(&(DRAG_DEVICE_B, wl_data_device::DATA_OFFER)),
        "an offer is made for the surface entered: {ops:?}"
    );
    assert!(ops.contains(&(DRAG_DEVICE_B, wl_data_device::ENTER)));
    assert_eq!(state.drag.as_ref().unwrap().focus, Some((2, 20)));
    assert_eq!(state.drag.as_ref().unwrap().focus_offers.len(), 1);

    // Off it again.
    state.cursor_x = 900.0;
    state.cursor_y = 700.0;
    assert!(update_drag(&mut state, 1));

    let ops = sent_ops(&mut target_rx);
    assert!(
        ops.contains(&(DRAG_DEVICE_B, wl_data_device::LEAVE)),
        "{ops:?}"
    );
    assert!(state.drag.as_ref().unwrap().focus.is_none());
}

#[test]
fn moving_within_one_surface_sends_motion_rather_than_re_entering() {
    let (mut state, _origin_rx, mut target_rx) = state_ready_to_drag();
    state.start_drag(Some((1, DRAG_SOURCE)), 1, WINDOW, None);
    state.cursor_x = 450.0;
    state.cursor_y = 50.0;
    update_drag(&mut state, 0);
    drop(sent_ops(&mut target_rx));

    state.cursor_x = 460.0;
    update_drag(&mut state, 1);

    let ops = sent_ops(&mut target_rx);
    assert_eq!(ops, vec![(DRAG_DEVICE_B, wl_data_device::MOTION)]);
}

#[test]
fn a_drag_with_no_source_reaches_only_the_client_that_started_it() {
    // An internal drag has nothing to hand anyone else, so nobody else is a
    // target.
    let (mut state, _origin_rx, mut target_rx) = state_ready_to_drag();
    state.start_drag(None, 1, WINDOW, None);

    state.cursor_x = 450.0;
    state.cursor_y = 50.0;
    update_drag(&mut state, 0);

    assert!(sent_ops(&mut target_rx).is_empty());
    assert!(state.drag.as_ref().unwrap().focus.is_none());
}

/// Put the drag over client 2's window and have that client accept it.
fn drag_accepted_over_the_target() -> (
    CompositorState,
    Receiver<WaylandProtocolMessage>,
    Receiver<WaylandProtocolMessage>,
) {
    let (mut state, origin_rx, target_rx) = state_ready_to_drag();
    state.start_drag(Some((1, DRAG_SOURCE)), 1, WINDOW, None);
    state.cursor_x = 450.0;
    state.cursor_y = 50.0;
    update_drag(&mut state, 0);

    let offer = state.drag.as_ref().unwrap().focus_offers[0];
    let entry = state.data_offers.get_mut(&offer).unwrap();
    entry.accepted = Some("text/plain".to_string());
    entry.actions = crate::compositor::protocol::wl_data_device_manager::DND_ACTION_COPY;
    entry.preferred_action = crate::compositor::protocol::wl_data_device_manager::DND_ACTION_COPY;
    state.resolve_offer_action(offer);
    (state, origin_rx, target_rx)
}

#[test]
fn releasing_over_an_accepting_target_drops() {
    let (mut state, mut origin_rx, mut target_rx) = drag_accepted_over_the_target();
    let offer = state.drag.as_ref().unwrap().focus_offers[0];
    drop(sent_ops(&mut origin_rx));
    drop(sent_ops(&mut target_rx));

    finish_drag(&mut state);

    assert!(
        sent_ops(&mut target_rx).contains(&(DRAG_DEVICE_B, wl_data_device::DROP)),
        "the target is told the user let go here"
    );
    assert!(
        sent_ops(&mut origin_rx).contains(&(
            DRAG_SOURCE,
            crate::compositor::protocol::wl_data_source::DND_DROP_PERFORMED
        )),
        "the source is told the drop happened"
    );
    // The pointer is free at once, but the offer lives on: the target still has
    // to read the data and say when it is done.
    assert!(state.drag.is_none());
    assert_eq!(state.data_offers[&offer].kind, OfferKind::Dropped);
    assert!(state.data_offers[&offer].source.is_some());
}

#[test]
fn releasing_over_a_target_that_accepted_nothing_cancels() {
    let (mut state, mut origin_rx, mut target_rx) = state_ready_to_drag();
    state.start_drag(Some((1, DRAG_SOURCE)), 1, WINDOW, None);
    state.cursor_x = 450.0;
    state.cursor_y = 50.0;
    update_drag(&mut state, 0);
    drop(sent_ops(&mut origin_rx));
    drop(sent_ops(&mut target_rx));

    finish_drag(&mut state);

    let target_ops = sent_ops(&mut target_rx);
    assert!(target_ops.contains(&(DRAG_DEVICE_B, wl_data_device::LEAVE)));
    assert!(!target_ops.contains(&(DRAG_DEVICE_B, wl_data_device::DROP)));
    assert!(
        sent_ops(&mut origin_rx).contains(&(
            DRAG_SOURCE,
            crate::compositor::protocol::wl_data_source::CANCELLED
        )),
        "a drag that lands nowhere cancels its source"
    );
}

#[test]
fn releasing_over_nothing_cancels() {
    let (mut state, mut origin_rx, _target_rx) = state_ready_to_drag();
    state.start_drag(Some((1, DRAG_SOURCE)), 1, WINDOW, None);
    state.cursor_x = 900.0;
    state.cursor_y = 700.0;
    update_drag(&mut state, 0);
    drop(sent_ops(&mut origin_rx));

    finish_drag(&mut state);

    assert!(sent_ops(&mut origin_rx).contains(&(
        DRAG_SOURCE,
        crate::compositor::protocol::wl_data_source::CANCELLED
    )));
}

#[test]
fn the_target_disconnecting_mid_drag_turns_the_drop_into_a_cancel() {
    let (mut state, mut origin_rx, _target_rx) = drag_accepted_over_the_target();
    state.remove_client_resources(2);
    state.clients.remove(2);
    drop(sent_ops(&mut origin_rx));

    // The button is still down, so the drag survives — with nobody to drop on.
    assert!(state.drag.is_some());
    assert!(state.drag.as_ref().unwrap().focus.is_none());

    finish_drag(&mut state);
    assert!(sent_ops(&mut origin_rx).contains(&(
        DRAG_SOURCE,
        crate::compositor::protocol::wl_data_source::CANCELLED
    )));
}

#[test]
fn the_origin_disconnecting_mid_drag_cancels_it() {
    let (mut state, _origin_rx, mut target_rx) = drag_accepted_over_the_target();
    drop(sent_ops(&mut target_rx));

    state.remove_client_resources(1);
    state.clients.remove(1);

    assert!(
        state.drag.is_none(),
        "there is nobody left to hand anything to"
    );
    assert!(sent_ops(&mut target_rx).contains(&(DRAG_DEVICE_B, wl_data_device::LEAVE)));
}

#[test]
fn destroying_the_origin_surface_cancels_the_drag() {
    let (mut state, _origin_rx, _target_rx) = drag_accepted_over_the_target();
    state.destroy_surface(1, WINDOW.1);
    assert!(state.drag.is_none());
}

#[test]
fn a_drag_and_an_interactive_grab_cannot_both_hold_the_pointer() {
    let (mut state, _origin_rx, _target_rx) = state_ready_to_drag();
    state.start_drag(Some((1, DRAG_SOURCE)), 1, WINDOW, None);

    state.start_move_grab(WINDOW);
    assert!(
        state.pointer_grab.is_none(),
        "a client cannot start a move off the same press that started the drag"
    );
}

#[test]
fn a_client_with_two_data_devices_is_entered_on_both() {
    let (mut state, _origin_rx, mut target_rx) = state_ready_to_drag();
    add_data_device(&mut state, 2, DRAG_DEVICE_B + 1);
    state.start_drag(Some((1, DRAG_SOURCE)), 1, WINDOW, None);
    drop(sent_ops(&mut target_rx));

    state.cursor_x = 450.0;
    state.cursor_y = 50.0;
    enter_drag_surface(&mut state, (2, 20), 10.0, 10.0);

    let ops = sent_ops(&mut target_rx);
    assert!(ops.contains(&(DRAG_DEVICE_B, wl_data_device::ENTER)));
    assert!(ops.contains(&(DRAG_DEVICE_B + 1, wl_data_device::ENTER)));
    assert_eq!(
        state.drag.as_ref().unwrap().focus_offers.len(),
        2,
        "an offer belongs to the device it arrived on"
    );
}
