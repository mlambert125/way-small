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
const DESCRIPTION: u16 = 5;

// Mode flags
pub const MODE_CURRENT: u32 = 1;

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
    let version = client.version(output_id);

    for output in state.outputs.iter() {
        let args = ArgWriter::new()
            .i32(output.geometry.x)
            .i32(output.geometry.y) // y
            .i32(output.geometry.physical_width)
            .i32(output.geometry.physical_height)
            .i32(output.geometry.subpixel as i32)
            .string(&output.geometry.make)
            .string(&output.geometry.model)
            .i32(output.geometry.transform as i32)
            .build();
        let _ = client.send(message(output_id, GEOMETRY, args)).await;

        for mode in output.modes.iter() {
            let args = ArgWriter::new()
                .u32(mode.flags as u32)
                .i32(mode.width)
                .i32(mode.height)
                .i32(mode.refresh_mhz as i32)
                .build();
            let _ = client.send(message(output_id, MODE, args)).await;
        }

        if version >= 2 {
            let args = ArgWriter::new().i32(output.scale).build();
            let _ = client.send(message(output_id, SCALE, args)).await;

            if version >= 4 {
                let args = ArgWriter::new().string(&output.name).build();
                let _ = client.send(message(output_id, NAME, args)).await;

                let args = ArgWriter::new().string(&output.description).build();
                let _ = client.send(message(output_id, DESCRIPTION, args)).await;
            }

            let _ = client.send(message(output_id, DONE, Vec::new())).await;
        }
    }
}

/// Send updated mode + done to all clients that have a bound wl_output.
pub async fn broadcast_mode(state: &mut CompositorState) {
    for output in state.outputs.iter() {
        for mode in output.modes.iter() {
            let args = ArgWriter::new()
                .u32(mode.flags as u32)
                .i32(mode.width)
                .i32(mode.height)
                .i32(mode.refresh_mhz as i32)
                .build();

            for (_, client) in state.clients.iter() {
                for (obj_id, obj_type) in client.objects.iter() {
                    if *obj_type == ObjectType::WlOutput {
                        let version = client.version(*obj_id);

                        let _ = client.send(message(*obj_id, MODE, args.clone())).await;
                        if version >= 2 {
                            let _ = client.send(message(*obj_id, DONE, Vec::new())).await;
                        }
                    }
                }
            }
        }
    }
}
