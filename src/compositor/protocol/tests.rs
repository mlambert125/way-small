//! Tests for request dispatch: what happens to a client that sends something
//! the compositor cannot make sense of.

use super::{CompositorState, handle_message};
use crate::compositor::protocol::wire_utils::ArgWriter;
use crate::wayland_socket::{WaylandProtocolMessage, WaylandProtocolMessageWithClientInfo};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;

const CLIENT: u32 = 1;
const SURFACE: u32 = 10;
const POSITIONER: u32 = 20;

/// A client with one surface, keeping hold of the pieces a test needs to see:
/// the token that says whether it has been disconnected, and the channel its
/// socket task would be draining.
fn client_with_a_surface() -> (
    CompositorState,
    CancellationToken,
    Receiver<WaylandProtocolMessage>,
) {
    let mut state = CompositorState::new();
    let (tx, rx) = channel(64);
    let token = CancellationToken::new();
    state.clients.create(
        CLIENT,
        tx,
        Arc::new(Mutex::new(VecDeque::new())),
        token.clone(),
    );
    state.create_surface(CLIENT, SURFACE);
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register(SURFACE, super::ObjectType::WlSurface)
        .unwrap();
    (state, token, rx)
}

fn deliver(state: &mut CompositorState, object_id: u32, op_code: u16, args: Vec<u8>) {
    handle_message(
        state,
        &WaylandProtocolMessageWithClientInfo {
            client_id: CLIENT,
            message: WaylandProtocolMessage {
                object_id,
                op_code,
                args,
                fds: Vec::new(),
            },
        },
    );
}

/// Whether the client was sent a `wl_display.error`.
fn was_sent_an_error(rx: &mut Receiver<WaylandProtocolMessage>) -> bool {
    std::iter::from_fn(|| rx.try_recv().ok()).any(|m| {
        m.object_id == super::wl_display::OBJECT_ID && m.op_code == super::wl_display::ERROR
    })
}

#[test]
fn an_opcode_the_interface_does_not_have_disconnects_the_client() {
    let (mut state, token, mut rx) = client_with_a_surface();

    // wl_surface's requests stop at 10. A client sending 99 is not speaking
    // the interface the compositor thinks this object is, and everything it
    // sends afterwards is decoded against that same disagreement.
    deliver(&mut state, SURFACE, 99, Vec::new());

    assert!(was_sent_an_error(&mut rx), "the client must be told why");
    assert!(token.is_cancelled(), "and it must be disconnected");
}

#[test]
fn a_request_short_of_its_arguments_disconnects_the_client() {
    let (mut state, token, mut rx) = client_with_a_surface();

    // wl_surface.frame carries a new_id. With no argument bytes at all there
    // is no callback to create and nothing to guess at.
    // (opcode 3 is wl_surface.frame)
    deliver(&mut state, SURFACE, 3, Vec::new());

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
}

#[test]
fn a_request_that_is_merely_unimplemented_is_not_fatal() {
    let (mut state, token, mut rx) = client_with_a_surface();
    state.create_xdg_positioner(CLIENT, POSITIONER);
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register(POSITIONER, super::ObjectType::XdgPositioner)
        .unwrap();

    // `set_reactive` is part of the xdg_wm_base version this compositor
    // advertises, so a client is entitled to send it. Nothing acts on it yet,
    // and that is not the client's problem — the distinction between a request
    // not implemented and a request that does not exist is the whole reason
    // unknown opcodes can be made fatal at all.
    deliver(&mut state, POSITIONER, 7, ArgWriter::new().u32(1).build());

    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
}
