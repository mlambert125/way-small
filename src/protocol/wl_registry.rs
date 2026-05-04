use tracing::debug;

use crate::wayland_socket::ClientMessage;

use super::{ArgReader, ArgWriter, ClientState, GLOBALS, message};

// Request opcodes
const BIND: u16 = 0;

// Event opcodes
pub const GLOBAL: u16 = 0;

pub async fn handle(state: &mut ClientState, msg: &ClientMessage) {
    match msg.message.op_code {
        BIND => handle_bind(state, msg).await,
        op => {
            tracing::warn!("wl_registry: unhandled opcode {}", op);
        }
    }
}

/// Send wl_registry.global events for all registered globals.
pub async fn advertise_globals(state: &mut ClientState, registry_id: u32) {
    for (name, global) in GLOBALS.iter().enumerate() {
        let args = ArgWriter::new()
            .u32(name as u32)
            .string(global.interface)
            .u32(global.version)
            .build();
        if state.send(message(registry_id, GLOBAL, args)).await.is_err() {
            return;
        }
    }
}

async fn handle_bind(state: &mut ClientState, msg: &ClientMessage) {
    let mut args = ArgReader::new(&msg.message.args);
    // bind args: u32 name, str interface, u32 version, u32 new_id
    let (Some(global_name), Some(interface), Some(_version), Some(new_id)) =
        (args.u32(), args.string(), args.u32(), args.new_id())
    else {
        state.send_error(msg.message.object_id, 0, "wl_registry.bind: malformed args").await;
        return;
    };

    if (global_name as usize) >= GLOBALS.len() {
        state.send_error(
            msg.message.object_id,
            0,
            &format!("wl_registry.bind: unknown global name {}", global_name),
        ).await;
        return;
    }

    let global = &GLOBALS[global_name as usize];
    debug!(
        "wl_registry.bind: name={} interface={} new_id={}",
        global_name, interface, new_id
    );

    // TODO: match on global.interface to register the right ObjectType
    tracing::warn!(
        "wl_registry.bind: no handler for interface '{}' yet",
        global.interface
    );
}
