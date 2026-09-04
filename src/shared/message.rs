//! What passes between the compositor and a backend.
//!
//! The two halves run as separate tasks and share no state, so everything one
//! knows about the other arrives as one of these: [`BackendRequest`] going out
//! from the compositor, [`BackendMessage`] coming back.

use super::clock::PresentedAt;
use super::dmabuf::{DmabufFormat, DmabufImage, DmabufProbe};
use super::input::{ButtonState, KeyState, MouseButton, ScrollSource};
use super::output::{Output, OutputId};
use std::sync::Arc;

/// A message from the backend to the compositor
#[derive(Debug)]
pub enum BackendMessage {
    /// A message reporting seat capabilities
    SeatCapabilities {
        /// A pointer is present and available
        pointer: bool,
        /// A keyboard is present and available
        keyboard: bool,
        /// A touchscreen is present and available
        touch: bool,
    },
    /// A message reporting output info
    OutputInfo {
        /// A vector of all available outputs
        outputs: Vec<Output>,
    },
    /// A message that the backend host has closed.  Only applicable to winit backend.
    Closed,
    /// The backend is ready to show another frame on this output, and is asking for one.
    FrameRequested(OutputId),
    /// A scene has reached the screen on this output.
    FramePresented(OutputId, PresentedAt),
    /// An output has been resized
    Resized(OutputId, i32, i32),
    /// A key has changed state (pressed/released)
    KeyInput {
        /// The keycode
        keycode: u32,
        /// The state of the key
        state: KeyState,
        /// A mask of which mod keys are pressed
        mods_depressed: u32,
        /// A mask of which mod keys are latched
        mods_latched: u32,
        /// A mask of which mod keys are locked
        mods_locked: u32,
        /// The active keyboard layout group index
        mods_group: u32,
    },
    /// The mouse has been moved (absolute)
    MouseMovedTo {
        /// The new x coordinate
        x: f64,
        /// The new y coordinate
        y: f64,
    },
    /// The mouse has moved by this much. (relative)
    #[allow(dead_code)]
    MouseMovedBy {
        /// The delta of x
        dx: f64,
        /// The delta of y
        dy: f64,
    },
    /// A mouse button has changed its state
    MouseButton {
        /// Which button changed
        button: MouseButton,
        /// The state of the button
        state: ButtonState,
    },
    /// A single finger has touched the screen.
    TouchDown {
        /// Which finger (id is unique among currently used, but doesn't really tell which.)
        id: i32,
        /// X coordinate for where it landed, in global compositor coordinates.
        x: f64,
        /// Y coordinate for where it landed, in global compositor coordinates.
        y: f64,
    },
    /// A finger already down has moved.
    TouchMotion {
        /// Which finger.
        id: i32,
        /// Its new position, in global compositor coordinates.
        x: f64,
        /// Likewise.
        y: f64,
    },
    /// A finger has been lifted.
    TouchUp {
        /// Which finger.
        id: i32,
    },
    /// The touch sequence has been taken over by something else
    TouchCancel,
    /// The pointer's scroll axes have moved.
    MouseScroll {
        /// The change in the x axis of the scroll
        dx: f64,
        /// The change in the y axis of the scroll
        dy: f64,
        /// What did the scrolling, which decides how a client should treat it.
        source: ScrollSource,
        /// Wheel detents on x axis, in 120ths of a click
        v120_x: i32,
        /// Wheel detents on y axis, in 120ths of a click
        v120_y: i32,
    },
    /// A continuous scroll has finished — the fingers have left the touchpad.
    MouseScrollEnd,
    /// What this backend can do with dma-bufs, in answer to
    /// [`BackendRequest::ProbeDmabuf`].
    DmabufSupport {
        /// Formats and modifiers that can be imported.
        formats: Vec<DmabufFormat>,
        /// What came of actually trying it.
        probe: DmabufProbe,
    },
    /// What came of a [`BackendRequest::ImportDmabuf`].
    DmabufImportResult {
        /// The token from the request.
        token: u64,
        /// Whether the driver took the buffer.
        imported: bool,
    },
    /// The host window has gained focus (winit only)
    FocusIn,
    /// The host window has lost focus (winit only)
    FocusOut,
}

/// A request from the compositor to the backend.
#[derive(Debug)]
pub enum BackendRequest {
    /// Report which dma-buf formats can be imported, having checked that
    /// importing actually works. Answered with [`BackendMessage::DmabufSupport`].
    ProbeDmabuf,
    /// Try importing one client buffer and report whether it took, answered
    /// with [`BackendMessage::DmabufImportResult`] carrying the same token.
    ImportDmabuf {
        /// Identifies this import. Never reused, so a late answer cannot be
        /// mistaken for the answer to a later question.
        token: u64,
        /// The buffer to try.
        image: Arc<DmabufImage>,
    },
}
