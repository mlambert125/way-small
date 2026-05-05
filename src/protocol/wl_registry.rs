//! wl_registry protocol handler.
//!
//! Advertises available globals to clients and handles bind requests,
//! which create new protocol objects for specific interfaces (wl_shm,
//! wl_compositor, xdg_wm_base, etc.).

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire::{ArgReader, ArgWriter, message};
use super::{ClientState, GLOBALS, ObjectType, wl_output, wl_seat, wl_shm};

// Request opcodes
const BIND: u16 = 0;

// Event opcodes
pub const GLOBAL: u16 = 0;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        BIND => handle_bind(state, msg).await,
        op => {
            tracing::warn!("wl_registry: unhandled opcode {}", op);
        }
    }
}

/// Send wl_registry.global events for all registered globals.
pub async fn advertise_globals(client: &mut ClientState, registry_id: u32) {
    for (name, global) in GLOBALS.iter().enumerate() {
        let args = ArgWriter::new()
            .u32(name as u32)
            .string(global.interface)
            .u32(global.version)
            .build();
        if client
            .send(message(registry_id, GLOBAL, args))
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn handle_bind(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // bind args: u32 name, str interface, u32 version, u32 new_id
    let (Some(global_name), Some(interface), Some(version), Some(new_id)) =
        (args.u32(), args.string(), args.u32(), args.new_id())
    else {
        let client = state.clients.get_or_create(msg.client_id);
        client
            .send_error(msg.message.object_id, 0, "wl_registry.bind: malformed args")
            .await;
        return;
    };

    if (global_name as usize) >= GLOBALS.len() {
        let client = state.clients.get_or_create(msg.client_id);
        client
            .send_error(
                msg.message.object_id,
                0,
                &format!("wl_registry.bind: unknown global name {}", global_name),
            )
            .await;
        return;
    }

    let global = &GLOBALS[global_name as usize];
    debug!(
        "wl_registry.bind: name={} interface={} new_id={}",
        global_name, interface, new_id
    );

    // Clamp to the version we actually support
    let bound_version = version.min(global.version);

    match global.interface {
        "wl_shm" => {
            let client = state.clients.get_or_create(msg.client_id);
            client.register_with_version(new_id, ObjectType::WlShm, bound_version);
            wl_shm::send_formats(client, new_id).await;
        }
        "wl_compositor" => {
            let client = state.clients.get_or_create(msg.client_id);
            client.register_with_version(new_id, ObjectType::WlCompositor, bound_version);
        }
        "wl_subcompositor" => {
            let client = state.clients.get_or_create(msg.client_id);
            client.register_with_version(new_id, ObjectType::WlSubcompositor, bound_version);
        }
        "wl_data_device_manager" => {
            let client = state.clients.get_or_create(msg.client_id);
            client.register_with_version(new_id, ObjectType::WlDataDeviceManager, bound_version);
        }
        "xdg_wm_base" => {
            let client = state.clients.get_or_create(msg.client_id);
            client.register_with_version(new_id, ObjectType::XdgWmBase, bound_version);
        }
        "xdg_system_bell_v1" => {
            let client = state.clients.get_or_create(msg.client_id);
            client.register_with_version(new_id, ObjectType::XdgSystemBell, bound_version);
        }
        "wl_seat" => {
            let client = state.clients.get_or_create(msg.client_id);
            client.register_with_version(new_id, ObjectType::WlSeat, bound_version);
            wl_seat::send_seat_info(state, msg.client_id, new_id).await;
        }
        "wl_output" => {
            let client = state.clients.get_or_create(msg.client_id);
            client.register_with_version(new_id, ObjectType::WlOutput, bound_version);
            wl_output::send_output_info(state, msg.client_id, new_id).await;
        }
        _ => {
            tracing::warn!(
                "wl_registry.bind: no handler for interface '{}' yet",
                global.interface
            );
        }
    }
}
