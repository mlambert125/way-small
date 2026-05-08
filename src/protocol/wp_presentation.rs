//! wp_presentation protocol handler.
//!
//! The wp_presentation global lets clients request per-commit feedback on
//! when their content was actually displayed. On bind the compositor sends
//! the clock_id event (CLOCK_MONOTONIC). Clients call feedback() to create
//! a wp_presentation_feedback object tied to the next commit on a surface.

use tracing::debug;

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::ObjectType;
use super::state::CompositorState;
use super::wire_utils::{ArgReader, ArgWriter, message};

// wp_presentation request opcodes
const DESTROY: u16 = 0;
const FEEDBACK: u16 = 1;

// wp_presentation event opcodes
const CLOCK_ID: u16 = 0;

pub async fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id).await;
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        FEEDBACK => handle_feedback(state, msg),
        op => {
            tracing::warn!("wp_presentation: unhandled opcode {}", op);
        }
    }
}

fn handle_feedback(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    let client = state.clients.get(msg.client_id);
    if client.is_none() {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    }
    let client = client.unwrap();
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(surface_id), Some(callback_id)) = (args.u32(), args.new_id()) else {
        return;
    };

    debug!(
        "wp_presentation.feedback: surface_id={} callback_id={}",
        surface_id, callback_id
    );

    client.register(callback_id, ObjectType::WpPresentationFeedback);

    // Store as pending; moved to committed list on wl_surface.commit
    if let Some(surface) = state.surfaces.get_mut(&(msg.client_id, surface_id)) {
        surface.pending.presentation_feedbacks.push(callback_id);
    }
}

/// Send the clock_id event to tell the client which clock we use.
pub async fn send_clock_id(state: &mut CompositorState, client_id: u32, object_id: u32) {
    // CLOCK_MONOTONIC = 1 on Linux
    let args = ArgWriter::new().u32(1).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(message(object_id, CLOCK_ID, args)).await;
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}
