//! Tests for scene building: how a surface's buffer, viewport and scale
//! become a quad, which output a window is drawn on, and how damage is
//! carried to the backend.

use super::{SceneCache, build};
use crate::compositor::protocol::CompositorState;
use crate::compositor::protocol::state::ViewportState;
use crate::shared::{
    OUTPUT_MODE_CURRENT, Output, OutputGeometry, OutputId, OutputMode, OutputSubpixel,
    OutputTransform,
};
use crate::shared::{
    PixelFormat, Scene, SceneElement, TextureId, TextureImage, TextureRect, TextureSource,
    UploadPixels,
};
use std::collections::VecDeque;
use std::os::fd::IntoRawFd;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::channel;
use tokio_util::sync::CancellationToken;

const SURFACE_COLOUR: u32 = 0xffff_0000;
const BUFFER_SIDE: i32 = 40;
const OUTPUT_SIDE: i32 = 100;
const OUTPUT: OutputId = OutputId(1);
const CLIENT: u32 = 1;
const POOL_ID: u32 = 100;
const BUFFER_ID: u32 = 101;
const SURFACE_ID: u32 = 200;
const VIEWPORT_ID: u32 = 300;

fn test_output() -> Output {
    Output {
        id: OUTPUT,
        geometry: OutputGeometry {
            x: 0,
            y: 0,
            physical_width: OUTPUT_SIDE,
            physical_height: OUTPUT_SIDE,
            subpixel: OutputSubpixel::None,
            make: String::new(),
            model: String::new(),
            transform: OutputTransform::Normal,
        },
        modes: vec![OutputMode {
            flags: OUTPUT_MODE_CURRENT,
            width: OUTPUT_SIDE,
            height: OUTPUT_SIDE,
            refresh_mhz: 60000,
        }],
        scale: 1,
        name: String::from("test"),
        description: String::from("test"),
    }
}

/// A compositor with one output and one client holding a solid-colour
/// `BUFFER_SIDE`-square buffer attached to a mapped surface at the origin.
fn test_state(buffer_scale: i32) -> CompositorState {
    let mut state = CompositorState::new();
    state.outputs.push(test_output());

    let (tx, _rx) = channel(64);
    state.clients.create(
        CLIENT,
        tx,
        Arc::new(Mutex::new(VecDeque::new())),
        CancellationToken::new(),
    );

    // A real memfd-backed pool, filled with a solid colour.
    let pixel_count = (BUFFER_SIDE * BUFFER_SIDE).unsigned_abs() as usize;
    let size = pixel_count * 4;
    let file = memfd_filled_with(size, SURFACE_COLOUR);
    state.register_shm_pool(
        CLIENT,
        POOL_ID,
        file.into_raw_fd(),
        size.try_into().unwrap(),
    );
    state.register_buffer(
        CLIENT,
        BUFFER_ID,
        POOL_ID,
        0,
        BUFFER_SIDE,
        BUFFER_SIDE,
        BUFFER_SIDE * 4,
        0,
    );

    state.create_surface(CLIENT, SURFACE_ID);
    let surface = state.surfaces.get_mut(&(CLIENT, SURFACE_ID)).unwrap();
    surface.buffer_id = Some(BUFFER_ID);
    surface.position = (0, 0);
    surface.buffer_scale = buffer_scale;
    // A window lives in a workspace, and only the workspace showing on an
    // output is drawn there.
    state.sync_workspaces();
    state.move_toplevel_to_output((CLIENT, SURFACE_ID), OUTPUT);

    state
}

fn memfd_filled_with(size: usize, colour: u32) -> std::fs::File {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::memfd_create(c"scene-test".as_ptr().cast(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let row: Vec<u8> = colour.to_ne_bytes().repeat(size / 4);
    file.write_all(&row).unwrap();
    file.flush().unwrap();
    file
}

/// Build a scene through a throwaway cache, for tests that only look at
/// one frame.
fn scene_of(state: &CompositorState) -> Scene {
    build(OUTPUT, state, &mut SceneCache::new())
}

/// The one element drawn from the client's buffer. The cursor is always in
/// the scene too, so it has to be filtered out rather than indexed past.
fn surface_element(scene: &Scene) -> &SceneElement {
    scene
        .elements
        .iter()
        .find(|e| e.texture.id == TextureId::Buffer(CLIENT, BUFFER_ID))
        .expect("no element for the client buffer")
}

#[test]
fn unscaled_buffer_covers_its_full_size() {
    let state = test_state(1);
    let scene = scene_of(&state);
    let element = surface_element(&scene);
    assert_eq!(element.dst, (0, 0, BUFFER_SIDE, BUFFER_SIDE));
    assert_eq!(
        element.src,
        (0.0, 0.0, f64::from(BUFFER_SIDE), f64::from(BUFFER_SIDE))
    );
}

#[test]
fn scaled_buffer_covers_its_logical_size() {
    // A scale-2 client submits a buffer twice as large in each axis, so the
    // same buffer must land on a quarter of the pixels — while still
    // sampling the whole of it.
    let state = test_state(2);
    let scene = scene_of(&state);
    let element = surface_element(&scene);
    assert_eq!(element.dst, (0, 0, BUFFER_SIDE / 2, BUFFER_SIDE / 2));
    assert_eq!(
        element.src,
        (0.0, 0.0, f64::from(BUFFER_SIDE), f64::from(BUFFER_SIDE))
    );
}

#[test]
fn viewport_crops_the_source_and_sets_the_destination() {
    let mut state = test_state(2);
    state.viewports.insert(
        (CLIENT, VIEWPORT_ID),
        ViewportState {
            client_id: CLIENT,
            surface_id: SURFACE_ID,
            source: Some((4.0, 8.0, 16.0, 20.0)),
            destination: Some((70, 30)),
            pending_source: None,
            pending_destination: None,
        },
    );
    state
        .surface_viewport
        .insert((CLIENT, SURFACE_ID), VIEWPORT_ID);

    let scene = scene_of(&state);
    let element = surface_element(&scene);
    assert_eq!(element.src, (4.0, 8.0, 16.0, 20.0));
    // The viewport destination wins outright — buffer scale does not apply
    // on top of it.
    assert_eq!(element.dst, (0, 0, 70, 30));
}

#[test]
fn surface_position_offsets_the_destination() {
    let mut state = test_state(1);
    state
        .surfaces
        .get_mut(&(CLIENT, SURFACE_ID))
        .unwrap()
        .position = (12, -7);
    let scene = scene_of(&state);
    assert_eq!(
        surface_element(&scene).dst,
        (12, -7, BUFFER_SIDE, BUFFER_SIDE)
    );
}

fn rect(x: i32, y: i32, width: i32, height: i32) -> TextureRect {
    TextureRect {
        x,
        y,
        width,
        height,
    }
}

/// The buffer's texture, built through a cache that persists across calls.
/// What an image says about the copy it is a patch against, and what changed.
///
/// Every image these tests build is an upload — there is no client here to
/// hand over a GPU buffer — so a dma-buf means the test built the wrong thing.
fn upload_of(image: &TextureImage) -> (Option<u64>, &[TextureRect]) {
    match &image.source {
        TextureSource::Upload {
            previous_serial,
            damage,
            ..
        } => (*previous_serial, damage),
        TextureSource::Dmabuf(_) => panic!("expected an uploaded image, not a dma-buf"),
    }
}

/// What changed in an image since the copy the backend holds.
fn damage_of(image: &TextureImage) -> &[TextureRect] {
    upload_of(image).1
}

/// The serial an image is a patch against, if it is one.
fn previous_serial_of(image: &TextureImage) -> Option<u64> {
    upload_of(image).0
}

fn texture_of(state: &CompositorState, cache: &mut SceneCache) -> Arc<TextureImage> {
    surface_element(&build(OUTPUT, state, cache))
        .texture
        .clone()
}

#[test]
fn an_idle_surface_keeps_its_serial() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();

    let first = texture_of(&state, &mut cache);
    let again = texture_of(&state, &mut cache);
    // A fresh handle each frame — there is no copy to reuse — but the same
    // serial, which is what tells the backend its texture is still good.
    assert_eq!(again.serial, first.serial);
    assert_eq!(previous_serial_of(&again), None);

    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[]);
    let after = texture_of(&state, &mut cache);
    assert!(after.serial > first.serial);
}

#[test]
fn damage_rides_along_as_a_patch_on_the_previous_copy() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();

    let first = texture_of(&state, &mut cache);
    // Nothing to patch against yet, so the whole image is the update.
    assert_eq!(previous_serial_of(&first), None);
    assert!(damage_of(&first).is_empty());
    state.clear_buffer_damage();

    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(4, 5, 6, 7)]);
    let second = texture_of(&state, &mut cache);
    // Anchored to the copy the backend is holding, so it can patch rather
    // than re-upload.
    assert_eq!(previous_serial_of(&second), Some(first.serial));
    assert_eq!(damage_of(&second), [rect(4, 5, 6, 7)]);
}

#[test]
fn damage_accumulates_until_it_is_consumed() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    texture_of(&state, &mut cache);
    state.clear_buffer_damage();

    // Two commits landing between two scenes must both survive: the second
    // says nothing about what the first changed.
    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(0, 0, 4, 4)]);
    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(8, 8, 4, 4)]);
    let image = texture_of(&state, &mut cache);
    assert_eq!(damage_of(&image), [rect(0, 0, 4, 4), rect(8, 8, 4, 4)]);
}

#[test]
fn undescribed_damage_widens_to_the_whole_buffer_and_stays_there() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    texture_of(&state, &mut cache);
    state.clear_buffer_damage();

    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(0, 0, 4, 4)]);
    // A change nobody could describe. No later rectangle can narrow the
    // window back down, because the undescribed change is still in it.
    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[]);
    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(8, 8, 4, 4)]);
    assert!(damage_of(&texture_of(&state, &mut cache)).is_empty());
}

#[test]
fn a_resized_buffer_cannot_be_patched() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    texture_of(&state, &mut cache);
    state.clear_buffer_damage();

    // Same buffer id, different shape: the rectangles no longer refer to
    // anything the backend holds, even where they overlap.
    state.register_buffer(
        CLIENT,
        BUFFER_ID,
        POOL_ID,
        0,
        BUFFER_SIDE / 2,
        BUFFER_SIDE / 2,
        (BUFFER_SIDE / 2) * 4,
        0,
    );
    state.mark_buffer_damaged(CLIENT, BUFFER_ID, &[rect(0, 0, 4, 4)]);
    assert!(damage_of(&texture_of(&state, &mut cache)).is_empty());
}

#[test]
fn dead_buffers_are_reaped_from_the_cache() {
    let mut state = test_state(1);
    let mut cache = SceneCache::new();
    texture_of(&state, &mut cache);

    state.destroy_buffer(CLIENT, BUFFER_ID);
    cache.gc(&state);
    assert!(cache.is_empty());
}

#[test]
fn buffer_pixels_are_borrowed_from_the_client_mapping() {
    let state = test_state(1);
    let texture = surface_element(&scene_of(&state)).texture.clone();

    assert_eq!(texture.format, PixelFormat::Argb8888);
    assert_eq!(texture.width, BUFFER_SIDE);
    assert_eq!(texture.height, BUFFER_SIDE);
    // Pointing at the client's pool, not at a copy of it.
    assert!(matches!(
        texture.source,
        TextureSource::Upload {
            pixels: UploadPixels::Mapped { .. },
            ..
        }
    ));

    // SAFETY: the image is alive, so nothing has been released.
    let bytes = unsafe { texture.bytes() }.expect("image does not fit its mapping");
    assert_eq!(
        bytes.len(),
        (BUFFER_SIDE * BUFFER_SIDE).unsigned_abs() as usize * 4
    );
    assert_eq!(bytes[..4], SURFACE_COLOUR.to_le_bytes());
}

#[test]
fn a_buffer_being_drawn_is_not_released_until_the_frame_goes() {
    let state = test_state(1);
    let key = (CLIENT, BUFFER_ID);
    // Nothing has borrowed it yet.
    assert!(!state.buffer_is_being_read(key));

    let scene = scene_of(&state);
    // The scene holds the only borrow, so the buffer is in use.
    assert!(state.buffer_is_being_read(key));

    drop(scene);
    // Frame gone, buffer free — this is what gates `wl_buffer.release`.
    assert!(!state.buffer_is_being_read(key));
}

#[test]
fn buffer_larger_than_its_pool_is_dropped() {
    let mut state = test_state(1);
    // Claim a buffer twice as tall as the pool can hold. Reading it would
    // run off the end of the mapping, so it must contribute no element.
    state.register_buffer(
        CLIENT,
        BUFFER_ID,
        POOL_ID,
        0,
        BUFFER_SIDE,
        BUFFER_SIDE * 2,
        BUFFER_SIDE * 4,
        0,
    );

    let scene = scene_of(&state);
    assert!(
        !scene
            .elements
            .iter()
            .any(|e| e.texture.id == TextureId::Buffer(CLIENT, BUFFER_ID))
    );
}

#[test]
fn cursor_is_drawn_when_no_client_has_set_one() {
    let mut state = test_state(1);
    state.cursor_x = 30.0;
    state.cursor_y = 40.0;

    let scene = scene_of(&state);
    let cursor = scene
        .elements
        .iter()
        .find(|e| e.texture.id == TextureId::FallbackCursor)
        .expect("no cursor in the scene");
    assert_eq!(cursor.dst.0, 30);
    assert_eq!(cursor.dst.1, 40);
    // Drawn on top of the surface it overlaps.
    assert_eq!(scene.elements.last().unwrap().texture.id, cursor.texture.id);
}

#[test]
fn client_can_hide_the_cursor() {
    let mut state = test_state(1);
    state.pointer_surface = Some((CLIENT, SURFACE_ID));
    state.cursor_surfaces.insert(CLIENT, None);

    let scene = scene_of(&state);
    assert!(!scene.elements.iter().any(|e| matches!(
        e.texture.id,
        TextureId::FallbackCursor | TextureId::DefaultCursor
    )));
}

const OUTPUT_B: OutputId = OutputId(2);

/// A second output placed to the right of the first.
fn add_second_output(state: &mut CompositorState) {
    let mut output = test_output();
    output.id = OUTPUT_B;
    output.geometry.x = OUTPUT_SIDE;
    state.outputs.push(output);
    // The new output arrives with a workspace of its own, empty until a
    // window is moved onto it.
    state.sync_workspaces();
}

#[test]
fn a_window_is_drawn_only_on_the_output_it_belongs_to() {
    let mut state = test_state(1);
    add_second_output(&mut state);
    let mut cache = SceneCache::new();

    // Belongs to the first output, so the second one draws nothing of it.
    assert!(
        build(OUTPUT, &state, &mut cache)
            .elements
            .iter()
            .any(|e| e.texture.id == TextureId::Buffer(CLIENT, BUFFER_ID))
    );
    assert!(
        !build(OUTPUT_B, &state, &mut cache)
            .elements
            .iter()
            .any(|e| e.texture.id == TextureId::Buffer(CLIENT, BUFFER_ID))
    );
}

#[test]
fn a_window_is_drawn_in_its_own_outputs_coordinates() {
    let mut state = test_state(1);
    add_second_output(&mut state);

    // Move it onto the second output, whose origin is OUTPUT_SIDE across.
    state.move_toplevel_to_output((CLIENT, SURFACE_ID), OUTPUT_B);
    let surface = state.surfaces.get_mut(&(CLIENT, SURFACE_ID)).unwrap();
    surface.position = (OUTPUT_SIDE + 10, 20);

    let scene = build(OUTPUT_B, &state, &mut SceneCache::new());
    // Global x of OUTPUT_SIDE + 10 is x = 10 on that output's own surface.
    assert_eq!(
        surface_element(&scene).dst,
        (10, 20, BUFFER_SIDE, BUFFER_SIDE)
    );
}

#[test]
fn the_cursor_is_drawn_only_on_the_output_under_it() {
    let mut state = test_state(1);
    add_second_output(&mut state);
    // Pointer sits on the second output.
    state.cursor_x = f64::from(OUTPUT_SIDE) + 30.0;
    state.cursor_y = 40.0;

    let is_cursor = |e: &SceneElement| {
        matches!(
            e.texture.id,
            TextureId::FallbackCursor | TextureId::DefaultCursor
        )
    };

    let first = build(OUTPUT, &state, &mut SceneCache::new());
    assert!(!first.elements.iter().any(is_cursor));

    let second = build(OUTPUT_B, &state, &mut SceneCache::new());
    let cursor = second
        .elements
        .iter()
        .find(|e| is_cursor(e))
        .expect("cursor should be on the output under it");
    // Translated into that output's coordinates.
    assert_eq!((cursor.dst.0, cursor.dst.1), (30, 40));
}

#[test]
fn a_window_is_confined_to_its_output() {
    let mut state = test_state(1);
    add_second_output(&mut state);

    // Shove it past the right edge of its own output, where it would
    // otherwise straddle into the second.
    state
        .surfaces
        .get_mut(&(CLIENT, SURFACE_ID))
        .unwrap()
        .position = (OUTPUT_SIDE - 5, 0);

    assert!(state.confine_toplevels(), "should have moved the window");
    let position = state.surfaces[&(CLIENT, SURFACE_ID)].position;
    assert_eq!(position, (OUTPUT_SIDE - BUFFER_SIDE, 0));
    // And it is still whole on its own output.
    assert!(position.0 + BUFFER_SIDE <= OUTPUT_SIDE);
}

#[test]
fn a_window_larger_than_its_output_is_pinned_to_the_corner() {
    let mut state = test_state(1);
    // Shrink the output below the window's size: nothing fits, so the
    // top-left is the useful part to show.
    for mode in &mut state.outputs[0].modes {
        mode.width = BUFFER_SIDE / 2;
        mode.height = BUFFER_SIDE / 2;
    }
    state.outputs[0].geometry.physical_width = BUFFER_SIDE / 2;
    state.outputs[0].geometry.physical_height = BUFFER_SIDE / 2;
    state
        .surfaces
        .get_mut(&(CLIENT, SURFACE_ID))
        .unwrap()
        .position = (30, 30);

    state.confine_toplevels();
    assert_eq!(state.surfaces[&(CLIENT, SURFACE_ID)].position, (0, 0));
}

#[test]
fn a_window_whose_output_vanished_is_rehomed() {
    let mut state = test_state(1);
    add_second_output(&mut state);
    // The output the window was on is unplugged, taking its workspace — and so
    // the window's home — with it.
    state.outputs.retain(|o| o.id != OUTPUT);

    assert!(state.confine_toplevels());
    let output = state.surface_output((CLIENT, SURFACE_ID));
    assert_eq!(
        output,
        Some(OUTPUT_B),
        "should have been re-homed onto the output that is left"
    );
}
