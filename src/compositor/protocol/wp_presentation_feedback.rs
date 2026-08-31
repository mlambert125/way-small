//! `wp_presentation_feedback` protocol handler.
//!
//! Feedback objects are one-shot: the compositor sends either a `presented`
//! or `discarded` event, then the object is defunct. Clients don't send
//! requests to this interface (no opcodes to handle).

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;

pub fn handle(_state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    tracing::warn!(
        "wp_presentation_feedback: unexpected request opcode {}",
        msg.message.op_code
    );
}
