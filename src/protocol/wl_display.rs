//! `wl_display` protocol handler (object id 1).
//!
//! Handles the two core requests every client makes first:
//! - sync: creates a transient `wl_callback`, fires done event, then deletes it
//! - `get_registry`: creates a `wl_registry` and advertises all globals

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::{ArgReader, ArgWriter, ObjectType, message, next_serial, wl_registry};

// The wl_display global is always object id 1 for every client.
pub const OBJECT_ID: u32 = 1;

// Request opcodes
const SYNC: u16 = 0;
const GET_REGISTRY: u16 = 1;

// Event opcodes
pub const ERROR: u16 = 0;
pub const DELETE_ID: u16 = 1;

// wl_callback event opcodes
const WL_CALLBACK_DONE: u16 = 0;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        SYNC => {
            let Some(client) = state.clients.get(msg.client_id) else {
                return;
            };
            handle_sync(client, msg).await;
        }
        GET_REGISTRY => handle_get_registry(state, msg).await,
        op => {
            tracing::warn!("wl_display: unhandled opcode {}", op);
        }
    }
}

async fn handle_sync(state: &mut super::ClientState, msg: &WaylandProtocolMessageWithClientInfo) {
    let Some(callback_id) = ArgReader::new(&msg.message.args).new_id() else {
        state
            .send_error(OBJECT_ID, 0, "wl_display.sync: missing callback id")
            .await;
        return;
    };
    debug!("wl_display.sync -> callback_id={}", callback_id);

    state.register(callback_id, ObjectType::WlCallback);

    let serial = next_serial();
    let args = ArgWriter::new().u32(serial).build();
    if state
        .send(message(callback_id, WL_CALLBACK_DONE, args))
        .await
        .is_err()
    {
        return;
    }

    state.unregister(callback_id).await;
}

async fn handle_get_registry(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let Some(client) = state.clients.get(msg.client_id) else {
        return;
    };

    let Some(registry_id) = ArgReader::new(&msg.message.args).new_id() else {
        client
            .send_error(OBJECT_ID, 0, "wl_display.get_registry: missing registry id")
            .await;
        return;
    };
    client.register(registry_id, ObjectType::WlRegistry);
    debug!("wl_display.get_registry -> registry_id={}", registry_id);

    wl_registry::advertise_globals(state, msg.client_id, registry_id).await;
}
