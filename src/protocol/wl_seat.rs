//! wl_seat protocol handler.
//!
//! A seat represents a group of input devices (pointer, keyboard, touch).
//! Clients bind wl_seat to receive capability events, then request specific
//! input device objects (wl_pointer, wl_keyboard).

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire::{ArgReader, ArgWriter, message};
use super::ObjectType;

// Request opcodes
const GET_POINTER: u16 = 0;
const GET_KEYBOARD: u16 = 1;
const GET_TOUCH: u16 = 2;
const RELEASE: u16 = 3;

// Event opcodes
pub const CAPABILITIES: u16 = 0;
pub const NAME: u16 = 1;

// Capability flags
const CAP_POINTER: u32 = 1;
const CAP_KEYBOARD: u32 = 2;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        GET_POINTER => handle_get_pointer(state, msg).await,
        GET_KEYBOARD => handle_get_keyboard(state, msg).await,
        GET_TOUCH => {
            debug!("wl_seat.get_touch: not supported");
        }
        RELEASE => {
            let client = state.clients.get_or_create(msg.client_id);
            client.unregister(msg.message.object_id);
        }
        op => {
            tracing::warn!("wl_seat: unhandled opcode {}", op);
        }
    }
}

/// Send wl_seat.capabilities and wl_seat.name after a client binds.
pub async fn send_seat_info(state: &mut CompositorState, client_id: u32, seat_id: u32) {
    let mut caps: u32 = 0;
    if state.seat.has_pointer {
        caps |= CAP_POINTER;
    }
    if state.seat.has_keyboard {
        caps |= CAP_KEYBOARD;
    }

    let client = state.clients.get_or_create(client_id);
    let args = ArgWriter::new().u32(caps).build();
    let _ = client.send(message(seat_id, CAPABILITIES, args)).await;

    let args = ArgWriter::new().string("default").build();
    let _ = client.send(message(seat_id, NAME, args)).await;
}

async fn handle_get_pointer(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(pointer_id) = args.new_id() else {
        let client = state.clients.get_or_create(msg.client_id);
        client
            .send_error(msg.message.object_id, 0, "wl_seat.get_pointer: malformed args")
            .await;
        return;
    };

    debug!("wl_seat.get_pointer: pointer_id={}", pointer_id);

    let client = state.clients.get_or_create(msg.client_id);
    client.register(pointer_id, ObjectType::WlPointer);
    state.pointers.push(super::state::PointerBinding {
        client_id: msg.client_id,
        object_id: pointer_id,
    });
}

async fn handle_get_keyboard(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(keyboard_id) = args.new_id() else {
        let client = state.clients.get_or_create(msg.client_id);
        client
            .send_error(msg.message.object_id, 0, "wl_seat.get_keyboard: malformed args")
            .await;
        return;
    };

    debug!("wl_seat.get_keyboard: keyboard_id={}", keyboard_id);

    let client = state.clients.get_or_create(msg.client_id);
    client.register(keyboard_id, ObjectType::WlKeyboard);
    state.keyboards.push(super::state::KeyboardBinding {
        client_id: msg.client_id,
        object_id: keyboard_id,
    });

    // Send keymap to the client (required before any key events)
    super::wl_keyboard::send_keymap(state, msg.client_id, keyboard_id).await;
}
