//! Protocol module root.
//!
//! Declares submodules, re-exports key types, defines shared protocol
//! constants (ObjectType, globals table, serial generation), and provides
//! the top-level handle_message() dispatch.

pub mod client;
pub mod state;
pub mod wire;
pub mod wl_buffer;
pub mod wl_callback;
pub mod wl_compositor;
pub mod wl_data_device;
pub mod wl_data_device_manager;
pub mod wl_data_offer;
pub mod wl_data_source;
pub mod wl_display;
pub mod wl_keyboard;
pub mod wl_output;
pub mod wl_pointer;
pub mod wl_region;
pub mod wl_registry;
pub mod wl_seat;
pub mod wl_shm;
pub mod wl_shm_pool;
pub mod wl_subcompositor;
pub mod wl_subsurface;
pub mod wl_surface;
pub mod wp_presentation;
pub mod wp_presentation_feedback;
pub mod wp_viewport;
pub mod wp_viewporter;
pub mod xdg_popup;
pub mod xdg_positioner;
pub mod xdg_surface;
pub mod xdg_system_bell;
pub mod xdg_toplevel;
pub mod xdg_wm_base;

use std::sync::atomic::{AtomicU32, Ordering};

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

// Re-export key types for convenience
pub use client::ClientState;
pub use state::CompositorState;
pub use wire::{ArgReader, ArgWriter, message};

static NEXT_SERIAL: AtomicU32 = AtomicU32::new(1);

pub fn next_serial() -> u32 {
    NEXT_SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// The type and state of a Wayland protocol object.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    WlDisplay,
    WlRegistry,
    WlCallback,
    WlShm,
    WlShmPool,
    WlBuffer,
    WlCompositor,
    WlSurface,
    WlRegion,
    WlSeat,
    WlPointer,
    WlKeyboard,
    WlOutput,
    WlSubcompositor,
    WlSubsurface,
    WlDataDeviceManager,
    WlDataDevice,
    WlDataSource,
    WlDataOffer,
    XdgWmBase,
    XdgSurface,
    XdgSystemBell,
    XdgToplevel,
    XdgPopup,
    XdgPositioner,
    WpPresentation,
    WpPresentationFeedback,
    WpViewporter,
    WpViewport,
}

/// A global interface that clients can bind via wl_registry.
pub struct Global {
    pub interface: &'static str,
    pub version: u32,
}

pub static GLOBALS: &[Global] = &[
    Global {
        interface: "wl_compositor",
        version: 5,
    },
    Global {
        interface: "wl_subcompositor",
        version: 1,
    },
    Global {
        interface: "wl_data_device_manager",
        version: 3,
    },
    Global {
        interface: "wl_shm",
        version: 1,
    },
    Global {
        interface: "wl_seat",
        version: 8,
    },
    Global {
        interface: "wl_output",
        version: 4,
    },
    Global {
        interface: "xdg_wm_base",
        version: 5,
    },
    Global {
        interface: "xdg_system_bell_v1",
        version: 1,
    },
    Global {
        interface: "wp_viewporter",
        version: 1,
    },
    Global {
        interface: "wp_presentation",
        version: 1,
    },
];

pub async fn handle_message(
    state: &mut CompositorState,
    message: &WaylandProtocolMessageWithClientInfo,
) {
    let object_id = message.message.object_id;

    // Ensure the client state has a sender (set on first message)
    {
        let client = state.clients.get_or_create(message.client_id);
        if client.sender.is_none() {
            client.sender = Some(message.socket_sender.clone());
        }
    }

    let obj_type = state
        .clients
        .get_or_create(message.client_id)
        .objects
        .get(&object_id)
        .copied();

    match obj_type {
        Some(ObjectType::WlDisplay) => {
            let client = state.clients.get_or_create(message.client_id);
            wl_display::handle(client, message).await;
        }
        Some(ObjectType::WlRegistry) => {
            wl_registry::handle(state, message).await;
        }
        Some(ObjectType::WlCallback) => {
            wl_callback::handle(message);
        }
        Some(ObjectType::WlShm) => {
            wl_shm::handle(state, message).await;
        }
        Some(ObjectType::WlShmPool) => {
            wl_shm_pool::handle(state, message).await;
        }
        Some(ObjectType::WlCompositor) => {
            wl_compositor::handle(state, message).await;
        }
        Some(ObjectType::WlSurface) => {
            wl_surface::handle(state, message).await;
        }
        Some(ObjectType::WlRegion) => {
            wl_region::handle(state, message).await;
        }
        Some(ObjectType::WlSubcompositor) => {
            wl_subcompositor::handle(state, message).await;
        }
        Some(ObjectType::WlSubsurface) => {
            wl_subsurface::handle(state, message).await;
        }
        Some(ObjectType::WlDataDeviceManager) => {
            wl_data_device_manager::handle(state, message).await;
        }
        Some(ObjectType::WlDataDevice) => {
            wl_data_device::handle(state, message).await;
        }
        Some(ObjectType::WlDataSource) => {
            wl_data_source::handle(state, message).await;
        }
        Some(ObjectType::WlDataOffer) => {
            wl_data_offer::handle(state, message).await;
        }
        Some(ObjectType::WlSeat) => {
            wl_seat::handle(state, message).await;
        }
        Some(ObjectType::WlPointer) => {
            wl_pointer::handle(state, message).await;
        }
        Some(ObjectType::WlKeyboard) => {
            wl_keyboard::handle(state, message).await;
        }
        Some(ObjectType::WlOutput) => {
            wl_output::handle(state, message).await;
        }
        Some(ObjectType::XdgWmBase) => {
            xdg_wm_base::handle(state, message).await;
        }
        Some(ObjectType::XdgSurface) => {
            xdg_surface::handle(state, message).await;
        }
        Some(ObjectType::XdgSystemBell) => {
            xdg_system_bell::handle(state, message);
        }
        Some(ObjectType::XdgToplevel) => {
            xdg_toplevel::handle(state, message).await;
        }
        Some(ObjectType::XdgPopup) => {
            xdg_popup::handle(state, message).await;
        }
        Some(ObjectType::XdgPositioner) => {
            xdg_positioner::handle(state, message).await;
        }
        Some(ObjectType::WlBuffer) => {
            wl_buffer::handle(state, message).await;
        }
        Some(ObjectType::WpPresentation) => {
            wp_presentation::handle(state, message).await;
        }
        Some(ObjectType::WpPresentationFeedback) => {
            wp_presentation_feedback::handle(state, message);
        }
        Some(ObjectType::WpViewporter) => {
            wp_viewporter::handle(state, message).await;
        }
        Some(ObjectType::WpViewport) => {
            wp_viewport::handle(state, message).await;
        }
        None => {
            tracing::warn!(
                "client {}: unknown object_id={}, op_code={}",
                message.client_id,
                object_id,
                message.message.op_code,
            );
            // WL_DISPLAY_ERROR_INVALID_OBJECT = 0
            let client = state.clients.get_or_create(message.client_id);
            client
                .send_error(object_id, 0, &format!("invalid object {}", object_id))
                .await;
        }
    }
}
