//! Tests for `wl_surface` damage: mapping a commit's damage rectangles
//! into the buffer pixels an upload can use.

use crate::compositor::protocol::wl_surface::committed_damage;
use crate::compositor::state::{CompositorState, ViewportState};
use crate::shared::TextureRect;

const CLIENT: u32 = 1;
const SURFACE: u32 = 10;
const BUFFER: u32 = 11;
const VIEWPORT: u32 = 12;
const SIDE: i32 = 40;

fn rect(x: i32, y: i32, width: i32, height: i32) -> TextureRect {
    TextureRect {
        x,
        y,
        width,
        height,
    }
}

/// A surface with a `SIDE`-square buffer attached at the given buffer scale.
fn surface_with_buffer(buffer_scale: i32) -> CompositorState {
    let mut state = CompositorState::new();
    state.create_surface(CLIENT, SURFACE);
    state.register_buffer(CLIENT, BUFFER, 0, 0, SIDE, SIDE, SIDE * 4, 0);
    let surface = state.surfaces.get_mut(&(CLIENT, SURFACE)).unwrap();
    surface.buffer_id = Some(BUFFER);
    surface.buffer_scale = buffer_scale;
    state
}

fn damage_for(
    state: &CompositorState,
    surface: &[TextureRect],
    buffer: &[TextureRect],
) -> Vec<TextureRect> {
    committed_damage(state, (CLIENT, SURFACE), BUFFER, surface, buffer)
}

#[test]
fn a_commit_that_reports_no_damage_means_all_of_it() {
    let state = surface_with_buffer(1);
    assert!(damage_for(&state, &[], &[]).is_empty());
}

#[test]
fn buffer_damage_is_taken_as_is_and_clipped() {
    let state = surface_with_buffer(1);
    // Already in buffer pixels, so only the overhang needs trimming.
    assert_eq!(
        damage_for(&state, &[], &[rect(4, 4, 8, 8), rect(38, 38, 10, 10)]),
        vec![rect(4, 4, 8, 8), rect(38, 38, 2, 2)]
    );
}

#[test]
fn surface_damage_is_scaled_into_buffer_pixels() {
    // At scale 2 one surface pixel is two buffer pixels, so a 10-square of
    // surface damage covers a 20-square of buffer — plus a pixel of pad on
    // each side, because the mapping is not pixel-exact.
    let state = surface_with_buffer(2);
    assert_eq!(
        damage_for(&state, &[rect(5, 5, 10, 10)], &[]),
        vec![rect(9, 9, 22, 22)]
    );
}

#[test]
fn surface_damage_follows_the_viewport() {
    // A viewport showing the buffer's top-left quarter blown up to the full
    // surface: surface coordinates are then half as large in buffer terms.
    let mut state = surface_with_buffer(1);
    state.viewports.insert(
        (CLIENT, VIEWPORT),
        ViewportState {
            client_id: CLIENT,
            surface_id: SURFACE,
            source: Some((0.0, 0.0, 20.0, 20.0)),
            destination: Some((40, 40)),
            pending_source: None,
            pending_destination: None,
        },
    );
    state.surface_viewport.insert((CLIENT, SURFACE), VIEWPORT);

    assert_eq!(
        damage_for(&state, &[rect(10, 10, 20, 20)], &[]),
        vec![rect(4, 4, 12, 12)]
    );
}

#[test]
fn damage_that_lands_entirely_outside_the_buffer_widens_to_everything() {
    let state = surface_with_buffer(1);
    // Nothing survives clipping, so there is no rectangle to promise with.
    // Falling back to a full upload is wasteful but never wrong.
    assert!(damage_for(&state, &[], &[rect(100, 100, 5, 5)]).is_empty());
}

#[test]
fn damage_on_a_surface_with_no_mapping_widens_to_everything() {
    let mut state = surface_with_buffer(1);
    state
        .surfaces
        .get_mut(&(CLIENT, SURFACE))
        .unwrap()
        .buffer_id = None;
    assert!(damage_for(&state, &[rect(0, 0, 4, 4)], &[]).is_empty());
}
