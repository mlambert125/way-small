//! wl_output protocol handler.
//!
//! Advertises display output properties to clients: geometry, mode,
//! scale factor, and name. Clients use this to position and size their
//! surfaces appropriately.

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::state::CompositorState;
use super::wire_utils::{ArgWriter, message};

// Request opcodes
const RELEASE: u16 = 0;

// Event opcodes
const GEOMETRY: u16 = 0;
const MODE: u16 = 1;
const DONE: u16 = 2;
const SCALE: u16 = 3;
const NAME: u16 = 4;

// Mode flags
const MODE_CURRENT: u32 = 1;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        RELEASE => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id).await;
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        op => {
            tracing::warn!("wl_output: unhandled opcode {}", op);
        }
    }
}

/// Send output info events after a client binds wl_output.
pub async fn send_output_info(state: &mut CompositorState, client_id: u32, output_id: u32) {
    let client = state.clients.get(client_id);
    if client.is_none() {
        tracing::warn!("Received message from unknown client {}", client_id);
        return;
    }
    let client = client.unwrap();

    let (width, height, refresh) = match &state.output {
        Some(o) => (o.width as i32, o.height as i32, o.refresh_mhz as i32),
        None => (800, 600, 60000),
    };

    // wl_output.geometry: x, y, physical_w_mm, physical_h_mm, subpixel, make, model, transform
    let args = ArgWriter::new()
        .i32(0) // x
        .i32(0) // y
        .i32(0) // physical width mm (unknown)
        .i32(0) // physical height mm (unknown)
        .i32(0) // subpixel: unknown
        .string("way-small")
        .string("virtual-output")
        .i32(0) // transform: normal
        .build();
    let _ = client.send(message(output_id, GEOMETRY, args)).await;

    // wl_output.mode: flags, width, height, refresh (mHz)
    let args = ArgWriter::new()
        .u32(MODE_CURRENT)
        .i32(width)
        .i32(height)
        .i32(refresh)
        .build();
    let _ = client.send(message(output_id, MODE, args)).await;

    // wl_output.scale (version 2+)
    let version = client.version(output_id);
    if version >= 2 {
        let args = ArgWriter::new().i32(1).build();
        let _ = client.send(message(output_id, SCALE, args)).await;
    }

    // wl_output.name (version 4+)
    if version >= 4 {
        let args = ArgWriter::new().string("WAY-SMALL-1").build();
        let _ = client.send(message(output_id, NAME, args)).await;
    }

    // wl_output.done (version 2+)
    if version >= 2 {
        let _ = client.send(message(output_id, DONE, Vec::new())).await;
    }
}

/// Send updated mode + done to all clients that have a bound wl_output.
pub async fn broadcast_mode(state: &mut CompositorState) {
    let (width, height, refresh) = match &state.output {
        Some(o) => (o.width as i32, o.height as i32, o.refresh_mhz as i32),
        None => return,
    };

    // Collect (client_id, output_object_id, version) for all bound wl_output objects
    let outputs: Vec<(u32, u32, u32)> = state
        .clients
        .iter()
        .flat_map(|(client_id, client)| {
            client
                .objects
                .iter()
                .filter(|(_, obj_type)| **obj_type == ObjectType::WlOutput)
                .map(move |(obj_id, _)| (*client_id, *obj_id, client.version(*obj_id)))
        })
        .collect();

    for (client_id, output_id, version) in outputs {
        let client = state.clients.get(client_id);
        if client.is_none() {
            continue;
        }
        let client = client.unwrap();

        // wl_output.mode
        let args = ArgWriter::new()
            .u32(MODE_CURRENT)
            .i32(width)
            .i32(height)
            .i32(refresh)
            .build();
        let _ = client.send(message(output_id, MODE, args)).await;

        // wl_output.done (version 2+)
        if version >= 2 {
            let _ = client.send(message(output_id, DONE, Vec::new())).await;
        }
    }
}
