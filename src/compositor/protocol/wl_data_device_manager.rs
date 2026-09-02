//! `wl_data_device_manager` protocol handler.
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

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        CREATE_DATA_SOURCE => handle_create_data_source(state, msg),
        GET_DATA_DEVICE => handle_get_data_device(state, msg),
        _ => super::unknown_request(state, msg, "wl_data_device_manager"),
    }
}

fn handle_create_data_source(
    state: &mut CompositorState,
    msg: &WaylandProtocolMessageWithClientInfo,
) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    let Some(source_id) = args.new_id() else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_data_device_manager.create_data_source: malformed args",
        );
        return;
    };

    debug!(
        "wl_data_device_manager.create_data_source: source_id={}",
        source_id
    );

    // Registering is the whole handler, so there is nothing to skip when the id
    // is rejected — `register` has already errored and dropped the client.
    let _ = client.register(source_id, ObjectType::WlDataSource);
}

fn handle_get_data_device(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    // get_data_device args: new_id, object seat
    let (Some(device_id), Some(_seat_id)) = (args.new_id(), args.u32()) else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_data_device_manager.get_data_device: malformed args",
        );
        return;
    };

    debug!(
        "wl_data_device_manager.get_data_device: device_id={}",
        device_id
    );

    // Registering is the whole handler, so there is nothing to skip when the id
    // is rejected — `register` has already errored and dropped the client.
    let _ = client.register(device_id, ObjectType::WlDataDevice);
}
