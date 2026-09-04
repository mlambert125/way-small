//! Tests for `wl_subcompositor`: the shape of the surface tree a client is
//! allowed to build.

use crate::compositor::protocol::wire_utils::ArgWriter;
use crate::compositor::protocol::wl_subcompositor::handle;
use crate::compositor::protocol::{ObjectType, wl_subsurface};
use crate::compositor::state::{CompositorState, MAX_SURFACE_OFFSET};
use crate::wayland_socket::{WaylandProtocolMessage, WaylandProtocolMessageWithClientInfo};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::channel;
use tokio_util::sync::CancellationToken;

const CLIENT: u32 = 1;
const SUBCOMPOSITOR: u32 = 2;
const PARENT: u32 = 10;
const CHILD: u32 = 11;
const SUBSURFACE: u32 = 12;
const GET_SUBSURFACE: u16 = 1;

fn state_with_two_surfaces() -> (CompositorState, CancellationToken) {
    let mut state = CompositorState::new();
    let (tx, _rx) = channel(64);
    let token = CancellationToken::new();
    state.clients.create(
        CLIENT,
        tx,
        Arc::new(Mutex::new(VecDeque::new())),
        token.clone(),
    );
    state.create_surface(CLIENT, PARENT);
    state.create_surface(CLIENT, CHILD);
    let client = state.clients.get(CLIENT).unwrap();
    client.register(PARENT, ObjectType::WlSurface).unwrap();
    client.register(CHILD, ObjectType::WlSurface).unwrap();
    client
        .register(SUBCOMPOSITOR, ObjectType::WlSubcompositor)
        .unwrap();
    (state, token)
}

fn get_subsurface(state: &mut CompositorState, id: u32, surface: u32, parent: u32) {
    handle(
        state,
        &WaylandProtocolMessageWithClientInfo {
            client_id: CLIENT,
            message: WaylandProtocolMessage {
                object_id: SUBCOMPOSITOR,
                op_code: GET_SUBSURFACE,
                args: ArgWriter::new().u32(id).u32(surface).u32(parent).build(),
                fds: Vec::new(),
            },
        },
    );
}

#[test]
fn a_surface_cannot_be_its_own_parent() {
    let (mut state, token) = state_with_two_surfaces();

    get_subsurface(&mut state, SUBSURFACE, PARENT, PARENT);

    // Left to stand, this is not a wrong position but a walk with no end:
    // finding the surface's global position loops, and composing or
    // hit-testing it recurses until the stack goes.
    assert!(token.is_cancelled());
    assert_eq!(state.surfaces.get(&(CLIENT, PARENT)).unwrap().parent, None);
    assert!(
        state
            .surfaces
            .get(&(CLIENT, PARENT))
            .unwrap()
            .children
            .is_empty(),
        "a refused request must leave nothing behind"
    );
}

#[test]
fn a_surface_cannot_be_made_its_own_grandparent() {
    let (mut state, token) = state_with_two_surfaces();

    // PARENT -> CHILD is fine.
    get_subsurface(&mut state, SUBSURFACE, CHILD, PARENT);
    assert!(!token.is_cancelled());

    // Closing the loop the other way round is the same cycle, one link longer.
    get_subsurface(&mut state, SUBSURFACE + 1, PARENT, CHILD);
    assert!(token.is_cancelled());
}

#[test]
fn a_wild_offset_is_brought_within_bounds() {
    let (mut state, _token) = state_with_two_surfaces();
    get_subsurface(&mut state, SUBSURFACE, CHILD, PARENT);

    wl_subsurface::handle(
        &mut state,
        &WaylandProtocolMessageWithClientInfo {
            client_id: CLIENT,
            message: WaylandProtocolMessage {
                object_id: SUBSURFACE,
                op_code: 1, // set_position
                args: ArgWriter::new().i32(i32::MAX).i32(i32::MIN).build(),
                fds: Vec::new(),
            },
        },
    );

    // The offset is added to a parent's, and that sum to an output origin.
    // Stored raw, it overflows the first of those additions.
    assert_eq!(
        state
            .surfaces
            .get(&(CLIENT, CHILD))
            .unwrap()
            .subsurface_position,
        (MAX_SURFACE_OFFSET, -MAX_SURFACE_OFFSET)
    );
}
