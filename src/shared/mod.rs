//! The vocabulary the subsystems share.
//!
//! Everything here crosses between the compositor and a backend: what the
//! backend reports (`BackendMessage`), what the compositor publishes to be
//! drawn (`Frame`), and the types those reach through — textures, the client
//! buffer memory behind them, and the outputs they land on.
//!
//! It lives above both because it belongs to neither. `Frame` is produced by
//! the compositor and consumed by a backend, so putting it in either would
//! make one depend on the other's internals for a type it owns half of. This
//! module depends on nothing in the crate, and everything else depends on it.

pub mod buffer;
pub mod output;
pub mod scene;
mod shm_guard;
pub mod texture;

pub use buffer::{BufferGuard, PoolMapping};
pub use output::{
    OUTPUT_MODE_CURRENT, OUTPUT_MODE_PREFERRED, Output, OutputGeometry, OutputId, OutputMode,
    OutputSubpixel, OutputTransform, cursor_bounds, output_contains,
};
pub use scene::{Frame, Scene, SceneElement};
pub use shm_guard::patched_pages;
pub use texture::{PixelFormat, TextureId, TextureImage, TexturePixels, TextureRect};

/// Background color for compositor
pub const BACKGROUND_COLOR: u32 = 0xff1a_1a2e;

/// Mouse button
#[derive(Debug, Clone, Copy)]
pub enum MouseButton {
    // Left mouse button
    Left,
    // Right mouse button
    Right,
    // Middle mouse button
    Middle,
}

/// State of a mouse button
#[derive(Debug, Clone, Copy)]
pub enum ButtonState {
    /// Button is pressed down
    Pressed,
    /// Button is released
    Released,
}

/// State of a keyboard key
#[derive(Debug, Clone, Copy)]
pub enum KeyState {
    /// Key is pressed down
    Pressed,
    /// Key is released
    Released,
}

/// A `CLOCK_MONOTONIC` instant, in the shape `wp_presentation_feedback` wants.
#[derive(Debug, Clone, Copy)]
pub struct PresentedAt {
    /// Whole seconds part
    pub tv_sec: i64,
    /// Nanosecond part
    pub tv_nsec: i64,
}

impl PresentedAt {
    /// Read the clock now. Called by a backend at the moment it presents, so
    /// the time a client is told is the backend's, not one measured a channel
    /// hop later in the compositor.
    pub fn now() -> Self {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `clock_gettime` only writes through the pointer it is given.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
        Self {
            tv_sec: ts.tv_sec,
            tv_nsec: ts.tv_nsec,
        }
    }
}

/// A message from the backend to the compositor
#[derive(Debug)]
pub enum BackendMessage {
    /// A message reporting seat capabilities
    SeatCapabilities {
        /// A pointer is present and available
        pointer: bool,
        /// A keyboard is present and available
        keyboard: bool,
    },
    /// A message reporting output info
    OutputInfo {
        /// A vector of all available outputs
        outputs: Vec<output::Output>,
    },
    /// A message that the backend host has closed.  Only applicable to winit backend
    /// or other future backends that can be closed by an externality
    Closed,
    /// A published frame has been put on screen.
    ///
    /// Clients pace themselves on `wl_surface.frame`, so this is what that
    /// callback should follow — not the earlier moment the compositor handed
    /// the frame over. Every backend sends it, including the headless one:
    /// a backend that never reported presenting would leave every client
    /// waiting forever for a callback that could not arrive.
    FramePresented(PresentedAt),
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
    /// The mouse has been moved
    /// The pointer is now at this position, in global coordinates.
    ///
    /// What a hosted backend reports, because the host owns the pointer and
    /// tells us where it put it, and what a touchscreen or a tablet produces.
    MouseMovedTo {
        /// The new x coordinate
        x: f64,
        /// The new y coordinate
        y: f64,
    },
    /// The pointer has moved by this much.
    ///
    /// What a mouse produces: at the `libinput` layer a mouse has no position
    /// at all, only movement. The compositor owns the position, accumulates
    /// these into it, and is responsible for keeping it on an output.
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
    /// The mouse scroll wheel has moved
    MouseScroll {
        /// The change in the x axis of the scroll
        dx: f64,
        /// The change in the y axis of the scroll
        dy: f64,
    },
    /// The host window has gained focus (winit only)
    FocusIn,
    /// The host window has lost focus (winit only)
    FocusOut,
}
