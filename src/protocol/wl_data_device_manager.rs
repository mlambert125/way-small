//! wl_data_device_manager protocol handler.
//!
//! The data device manager global lets clients create data sources (for
//! offering clipboard/drag content) and get data devices (for receiving
//! selections and drag events).

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::state::CompositorState;
use super::wire_utils::ArgReader;

// Request opcodes
const CREATE_DATA_SOURCE: u16 = 0;
const GET_DATA_DEVICE: u16 = 1;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        CREATE_DATA_SOURCE => handle_create_data_source(state, msg).await,
        GET_DATA_DEVICE => handle_get_data_device(state, msg).await,
        op => {
            tracing::warn!("wl_data_device_manager: unhandled opcode {}", op);
        }
    }
}

async fn handle_create_data_source(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    let Some(source_id) = args.new_id() else {
        client
            .send_error(
                msg.message.object_id,
                0,
                "wl_data_device_manager.create_data_source: malformed args",
            )
            .await;
        return;
    };

    debug!(
        "wl_data_device_manager.create_data_source: source_id={}",
        source_id
    );

    client.register(source_id, ObjectType::WlDataSource);
}

async fn handle_get_data_device(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    // get_data_device args: new_id, object seat
    let (Some(device_id), Some(_seat_id)) = (args.new_id(), args.u32()) else {
        client
            .send_error(
                msg.message.object_id,
                0,
                "wl_data_device_manager.get_data_device: malformed args",
            )
            .await;
        return;
    };

    debug!(
        "wl_data_device_manager.get_data_device: device_id={}",
        device_id
    );

    client.register(device_id, ObjectType::WlDataDevice);
}
