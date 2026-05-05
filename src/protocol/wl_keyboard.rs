//! wl_keyboard protocol handler.
//!
//! Delivers keyboard events to focused clients: keymap, enter, leave,
//! key, and modifiers events.

use std::io::Write;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::next_serial;
use super::state::CompositorState;
use super::wire::{ArgWriter, message};
use crate::wayland_socket::WaylandProtocolMessage;
use std::collections::VecDeque;

// Request opcodes
const RELEASE: u16 = 0;

// Event opcodes
pub const KEYMAP: u16 = 0;
pub const ENTER: u16 = 1;
pub const LEAVE: u16 = 2;
pub const KEY: u16 = 3;
pub const MODIFIERS: u16 = 4;

// Keymap format
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        RELEASE => {
            let keyboard_id = msg.message.object_id;
            state.keyboards.retain(|k| k.object_id != keyboard_id);
            let client = state.clients.get_or_create(msg.client_id);
            client.unregister(keyboard_id);
        }
        op => {
            tracing::warn!("wl_keyboard: unhandled opcode {}", op);
        }
    }
}

/// Generate the default XKB keymap string.
fn generate_keymap() -> Option<String> {
    let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let keymap = xkbcommon::xkb::Keymap::new_from_names(
        &context,
        "",
        "",
        "",
        "",
        None,
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )?;
    Some(keymap.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1))
}

/// Send the XKB keymap to a client's keyboard object via an FD.
pub async fn send_keymap(state: &mut CompositorState, client_id: u32, keyboard_id: u32) {
    let Some(keymap_str) = generate_keymap() else {
        tracing::warn!("Failed to create default xkb keymap");
        return;
    };
    let keymap_size = keymap_str.len() + 1; // include null terminator

    // Write keymap to a memfd
    let fd = unsafe { libc::memfd_create(c"keymap".as_ptr() as *const _, 0) };
    if fd < 0 {
        tracing::warn!("Failed to create memfd for keymap");
        return;
    }

    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    if file.write_all(keymap_str.as_bytes()).is_err() || file.write_all(&[0]).is_err() {
        tracing::warn!("Failed to write keymap to memfd");
        return;
    }
    let fd = std::os::unix::io::IntoRawFd::into_raw_fd(file);

    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };

    let args = ArgWriter::new()
        .u32(KEYMAP_FORMAT_XKB_V1)
        .u32(keymap_size as u32)
        .build();

    let msg = WaylandProtocolMessage {
        object_id: keyboard_id,
        op_code: KEYMAP,
        args,
        fds: VecDeque::from([fd]),
    };

    let client = state.clients.get_or_create(client_id);
    let _ = client.send(msg).await;
}

/// Send wl_keyboard.enter to a client's keyboard object.
pub async fn send_enter(
    state: &mut CompositorState,
    client_id: u32,
    keyboard_id: u32,
    surface_id: u32,
) {
    let serial = next_serial();
    // keys array is empty (no keys currently pressed)
    let args = ArgWriter::new()
        .u32(serial)
        .u32(surface_id)
        .u32(0) // wl_array length = 0
        .build();
    let client = state.clients.get_or_create(client_id);
    let _ = client.send(message(keyboard_id, ENTER, args)).await;
}

/// Send wl_keyboard.leave to a client's keyboard object.
pub async fn send_leave(
    state: &mut CompositorState,
    client_id: u32,
    keyboard_id: u32,
    surface_id: u32,
) {
    let serial = next_serial();
    let args = ArgWriter::new().u32(serial).u32(surface_id).build();
    let client = state.clients.get_or_create(client_id);
    let _ = client.send(message(keyboard_id, LEAVE, args)).await;
}

/// Send wl_keyboard.key to a client's keyboard object.
pub async fn send_key(
    state: &mut CompositorState,
    client_id: u32,
    keyboard_id: u32,
    time_ms: u32,
    key: u32,
    pressed: bool,
) {
    let serial = next_serial();
    let key_state: u32 = if pressed { 1 } else { 0 };
    let args = ArgWriter::new()
        .u32(serial)
        .u32(time_ms)
        .u32(key)
        .u32(key_state)
        .build();
    let client = state.clients.get_or_create(client_id);
    let _ = client.send(message(keyboard_id, KEY, args)).await;
}

/// Send wl_keyboard.modifiers to a client's keyboard object.
pub async fn send_modifiers(
    state: &mut CompositorState,
    client_id: u32,
    keyboard_id: u32,
    mods_depressed: u32,
    mods_latched: u32,
    mods_locked: u32,
    group: u32,
) {
    let serial = next_serial();
    let args = ArgWriter::new()
        .u32(serial)
        .u32(mods_depressed)
        .u32(mods_latched)
        .u32(mods_locked)
        .u32(group)
        .build();
    let client = state.clients.get_or_create(client_id);
    let _ = client.send(message(keyboard_id, MODIFIERS, args)).await;
}

use std::os::unix::io::FromRawFd;
