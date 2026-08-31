//! Protocol module root.
//!
//! Declares submodules, re-exports key types, defines shared protocol
//! constants (`ObjectType`, globals table, serial generation), and provides
//! the top-level `handle_message()` dispatch.

pub mod client;
pub mod state;
pub mod wire_utils;
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

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

// Re-export key types for convenience
pub use client::ClientState;
pub use state::CompositorState;
pub use wire_utils::{ArgReader, ArgWriter, message};

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

/// A global interface that clients can bind via `wl_registry`.
pub struct Global {
    pub interface: &'static str,
    pub version: u32,
}

/// The `wl_output` version we support. Defined here since `wl_output` globals
/// are dynamically advertised (one per physical output) rather than being
/// in the static GLOBALS array.
pub const WL_OUTPUT_VERSION: u32 = 4;

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
    // wl_output is not in this static list — each physical output gets its own
    // dynamic global, managed via CompositorState::output_global_names.
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

/// Number of file descriptors a request carries as ancillary data.
///
/// Wayland passes fds out-of-band, so the socket task cannot pair them with
/// messages on its own — that needs the object id to interface mapping, which
/// only lives here. This table is the single place that pairing is decided, and
/// `handle_message` applies it to every request before dispatch.
///
/// Any new request taking an `fd` argument MUST be listed here, and its
/// interface's arm in `handle_message` widened to receive the fds. Forgetting
/// the table entry desyncs the client's fd queue for the rest of the
/// connection; forgetting only the arm merely closes the fd, which makes the
/// request a no-op but keeps every later request correct.
fn request_fd_count(obj_type: ObjectType, op_code: u16) -> usize {
    match (obj_type, op_code) {
        (ObjectType::WlShm, wl_shm::CREATE_POOL)
        | (ObjectType::WlDataOffer, wl_data_offer::RECEIVE) => 1,
        _ => 0,
    }
}

#[allow(clippy::too_many_lines)]
pub fn handle_message(state: &mut CompositorState, message: &WaylandProtocolMessageWithClientInfo) {
    let object_id = message.message.object_id;
    let client_id = message.client_id;
    let Some(client) = state.clients.get(client_id) else {
        tracing::warn!("Received message from unknown client {}", message.client_id);
        return;
    };

    let obj_type = client.objects.get(&object_id).copied();

    // Claim the fds this request is specified to carry, before any handler
    // runs. Holding them here rather than leaving them on the shared queue is
    // what makes the accounting total: a handler that ignores them (or returns
    // early, or is an unimplemented stub) drops the `Vec<OwnedFd>` and the
    // descriptors are closed, instead of being mispaired with a later request.
    let mut request_fds: Vec<OwnedFd> = Vec::new();
    if let Some(obj_type) = obj_type {
        let count = request_fd_count(obj_type, message.message.op_code);
        if count > 0 {
            let mut queue = client.fd_queue.lock().unwrap();
            if queue.len() < count {
                // An fd always reaches us with the first byte of the message it
                // belongs to, so a short queue is a genuine protocol violation
                // rather than a race with the socket task. Continuing would
                // mispair every later fd, so drop the client.
                drop(queue);
                tracing::warn!(
                    "client {}: request for object {} is missing its file descriptor",
                    client_id,
                    object_id,
                );
                // WL_DISPLAY_ERROR_INVALID_METHOD = 1
                client.send_error(object_id, 1, "request is missing its file descriptor");
                client.cancel_token.cancel();
                return;
            }
            request_fds = queue.drain(..count).collect();
        }
    }

    match obj_type {
        Some(ObjectType::WlDisplay) => {
            wl_display::handle(state, message);
        }
        Some(ObjectType::WlRegistry) => {
            wl_registry::handle(state, message);
        }
        Some(ObjectType::WlCallback) => {
            wl_callback::handle(message);
        }
        Some(ObjectType::WlShm) => {
            wl_shm::handle(state, message, request_fds);
        }
        Some(ObjectType::WlShmPool) => {
            wl_shm_pool::handle(state, message);
        }
        Some(ObjectType::WlCompositor) => {
            wl_compositor::handle(state, message);
        }
        Some(ObjectType::WlSurface) => {
            wl_surface::handle(state, message);
        }
        Some(ObjectType::WlRegion) => {
            wl_region::handle(state, message);
        }
        Some(ObjectType::WlSubcompositor) => {
            wl_subcompositor::handle(state, message);
        }
        Some(ObjectType::WlSubsurface) => {
            wl_subsurface::handle(state, message);
        }
        Some(ObjectType::WlDataDeviceManager) => {
            wl_data_device_manager::handle(state, message);
        }
        Some(ObjectType::WlDataDevice) => {
            wl_data_device::handle(state, message);
        }
        Some(ObjectType::WlDataSource) => {
            wl_data_source::handle(state, message);
        }
        Some(ObjectType::WlDataOffer) => {
            wl_data_offer::handle(state, message, request_fds);
        }
        Some(ObjectType::WlSeat) => {
            wl_seat::handle(state, message);
        }
        Some(ObjectType::WlPointer) => {
            wl_pointer::handle(state, message);
        }
        Some(ObjectType::WlKeyboard) => {
            wl_keyboard::handle(state, message);
        }
        Some(ObjectType::WlOutput) => {
            wl_output::handle(state, message);
        }
        Some(ObjectType::XdgWmBase) => {
            xdg_wm_base::handle(state, message);
        }
        Some(ObjectType::XdgSurface) => {
            xdg_surface::handle(state, message);
        }
        Some(ObjectType::XdgSystemBell) => {
            xdg_system_bell::handle(state, message);
        }
        Some(ObjectType::XdgToplevel) => {
            xdg_toplevel::handle(state, message);
        }
        Some(ObjectType::XdgPopup) => {
            xdg_popup::handle(state, message);
        }
        Some(ObjectType::XdgPositioner) => {
            xdg_positioner::handle(state, message);
        }
        Some(ObjectType::WlBuffer) => {
            wl_buffer::handle(state, message);
        }
        Some(ObjectType::WpPresentation) => {
            wp_presentation::handle(state, message);
        }
        Some(ObjectType::WpPresentationFeedback) => {
            wp_presentation_feedback::handle(state, message);
        }
        Some(ObjectType::WpViewporter) => {
            wp_viewporter::handle(state, message);
        }
        Some(ObjectType::WpViewport) => {
            wp_viewport::handle(state, message);
        }
        None => {
            tracing::warn!(
                "client {}: unknown object_id={}, op_code={}",
                message.client_id,
                object_id,
                message.message.op_code,
            );
            // WL_DISPLAY_ERROR_INVALID_OBJECT = 0
            client.send_error(object_id, 0, &format!("invalid object {object_id}"));
            // Fatal by spec. Also necessary here: we cannot know how many fds an
            // unknown object's request carried, so anything it attached is
            // already orphaned on the queue and would mispair later requests.
            client.cancel_token.cancel();
        }
    }
}
