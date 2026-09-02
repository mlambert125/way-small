//! `wl_registry` protocol handler.
//!
//! Advertises available globals to clients and handles bind requests,
//! which create new protocol objects for specific interfaces (`wl_shm`,
//! `wl_compositor`, `xdg_wm_base`, etc.).

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;
use super::wire_utils::{ArgReader, ArgWriter, message};
use super::{GLOBALS, ObjectType, wl_output, wl_seat, wl_shm, zwp_linux_dmabuf};
use crate::shared::OutputId;

// Request opcodes
const BIND: u16 = 0;

// Event opcodes
pub const GLOBAL: u16 = 0;

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        BIND => handle_bind(state, msg),
        _ => super::unknown_request(state, msg, "wl_registry"),
    }
}

fn handle_bind(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    // Pre-collect output global mappings to avoid borrow conflicts later.
    let output_globals: Vec<(u32, OutputId)> = state
        .output_global_names
        .iter()
        .map(|(&id, &name)| (name, id))
        .collect();
    let dmabuf_global = state.dmabuf_global_name;

    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    // bind args: u32 name, str interface, u32 version, u32 new_id
    let (Some(global_name), Some(interface), Some(version), Some(new_id)) =
        (args.u32(), args.string(), args.u32(), args.new_id())
    else {
        client.send_error(msg.message.object_id, 0, "wl_registry.bind: malformed args");
        return;
    };

    debug!(
        "wl_registry.bind: name={} interface={} new_id={}",
        global_name, interface, new_id
    );

    // Check static globals first.
    if (global_name as usize) < GLOBALS.len() {
        let global = &GLOBALS[global_name as usize];
        let bound_version = version.min(global.version);

        // One register for every static global; only the interface differs.
        let object_type = match global.interface {
            "wl_shm" => ObjectType::WlShm,
            "wl_compositor" => ObjectType::WlCompositor,
            "wl_subcompositor" => ObjectType::WlSubcompositor,
            "wl_data_device_manager" => ObjectType::WlDataDeviceManager,
            "xdg_wm_base" => ObjectType::XdgWmBase,
            "xdg_system_bell_v1" => ObjectType::XdgSystemBell,
            "wl_seat" => ObjectType::WlSeat,
            "wp_viewporter" => ObjectType::WpViewporter,
            "wp_presentation" => ObjectType::WpPresentation,
            other => {
                tracing::warn!("wl_registry.bind: no handler for interface '{}' yet", other);
                return;
            }
        };

        if client
            .register_with_version(new_id, object_type, bound_version)
            .is_err()
        {
            return;
        }

        // Interfaces that push initial state to the client on bind.
        match object_type {
            ObjectType::WlShm => wl_shm::send_formats(client, new_id),
            ObjectType::WlSeat => wl_seat::send_seat_info(state, msg.client_id, new_id),
            ObjectType::WpPresentation => {
                super::wp_presentation::send_clock_id(state, msg.client_id, new_id);
            }
            _ => {}
        }
    } else if dmabuf_global == Some(global_name) {
        let bound_version = version.min(zwp_linux_dmabuf::VERSION);
        if client
            .register_with_version(new_id, ObjectType::ZwpLinuxDmabuf, bound_version)
            .is_err()
        {
            return;
        }
        // NLL: client borrow ends here. The format list lives in state, which
        // is why this cannot be sent through the borrow above.
        zwp_linux_dmabuf::send_formats(state, msg.client_id, new_id);
    } else if let Some(&(_, output_id)) =
        output_globals.iter().find(|(name, _)| *name == global_name)
    {
        // Dynamic output global — bind to the specific output.
        let bound_version = version.min(super::WL_OUTPUT_VERSION);
        if client
            .register_with_version(new_id, ObjectType::WlOutput, bound_version)
            .is_err()
        {
            return;
        }
        // NLL: client borrow ends here
        state
            .output_bindings
            .insert((msg.client_id, new_id), output_id);
        wl_output::send_output_info(state, msg.client_id, new_id, output_id);
    } else {
        client.send_error(
            msg.message.object_id,
            0,
            &format!("wl_registry.bind: unknown global name {global_name}"),
        );
    }
}

/// Send `wl_registry.global` events for all static globals and dynamic output globals.
pub fn advertise_globals(state: &mut CompositorState, client_id: u32, registry_id: u32) {
    // Collect output global names before borrowing client.
    let output_globals: Vec<u32> = state.output_global_names.values().copied().collect();
    let dmabuf_global = state.dmabuf_global_name;

    let Some(client) = state.clients.get(client_id) else {
        return;
    };

    // Static globals
    for (id, global) in (0u32..).zip(GLOBALS.iter()) {
        let args = ArgWriter::new()
            .u32(id)
            .string(global.interface)
            .u32(global.version)
            .build();
        if client.send(message(registry_id, GLOBAL, args)).is_err() {
            return;
        }
    }

    // Advertised only once the backend has said it can import one, which may
    // be after this client connected — hence the broadcast path as well.
    if let Some(global_name) = dmabuf_global {
        let args = ArgWriter::new()
            .u32(global_name)
            .string(zwp_linux_dmabuf::INTERFACE)
            .u32(zwp_linux_dmabuf::VERSION)
            .build();
        if client.send(message(registry_id, GLOBAL, args)).is_err() {
            return;
        }
    }

    // Dynamic output globals (one per physical output)
    for global_name in output_globals {
        let args = ArgWriter::new()
            .u32(global_name)
            .string("wl_output")
            .u32(super::WL_OUTPUT_VERSION)
            .build();
        if client.send(message(registry_id, GLOBAL, args)).is_err() {
            return;
        }
    }
}

/// Broadcast a global that has appeared since the clients did.
///
/// Most globals exist before any client connects and are listed in
/// `advertise_globals`. Some cannot: an output arrives when the display is
/// plugged in, and dma-buf support is only known once the backend has a GL
/// context to ask. Those are announced to whoever is already connected here,
/// and by `advertise_globals` to whoever connects later.
pub fn broadcast_global(
    state: &mut CompositorState,
    global_name: u32,
    interface: &str,
    version: u32,
) {
    for (_, client) in state.clients.iter() {
        for (obj_id, obj_type) in &client.objects {
            if *obj_type == ObjectType::WlRegistry {
                let args = ArgWriter::new()
                    .u32(global_name)
                    .string(interface)
                    .u32(version)
                    .build();
                let _ = client.send(message(*obj_id, GLOBAL, args));
            }
        }
    }
}
