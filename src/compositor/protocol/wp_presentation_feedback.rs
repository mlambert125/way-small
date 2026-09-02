//! `wp_presentation_feedback` protocol handler.
//!
//! Feedback objects are one-shot: the compositor sends either a `presented`
//! or `discarded` event, then the object is defunct. Clients don't send
//! requests to this interface (no opcodes to handle).

use crate::wayland_socket::WaylandProtocolMessageWithClientInfo;

use super::state::CompositorState;

// Event opcodes
/// `sync_output(output: object<wl_output>)`. Optional, and never sent: it
/// names the output a frame was synchronised to, which this compositor does
/// not track per frame.
#[allow(dead_code)]
pub const SYNC_OUTPUT: u16 = 0;
/// `presented(tv_sec_hi, tv_sec_lo, tv_nsec, refresh, seq_hi, seq_lo, flags)`.
pub const PRESENTED: u16 = 1;
/// `discarded()`, for a frame that never made it to screen.
#[allow(dead_code)]
pub const DISCARDED: u16 = 2;

pub fn handle(state: &mut CompositorState, msg: &WaylandProtocolMessageWithClientInfo) {
    super::unknown_request(state, msg, "wp_presentation_feedback");
}
