//! The vocabulary the subsystems share.

pub mod buffer;
pub mod dmabuf;
pub mod output;
pub mod scene;
mod shm_guard;
pub mod texture;

pub use buffer::{BufferGuard, PoolMapping};
pub use dmabuf::{
    DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888,
    DmabufFormat, DmabufImage, DmabufPlane, DmabufProbe, fourcc_name, pixel_format,
};
pub use output::{
    OUTPUT_MODE_CURRENT, OUTPUT_MODE_PREFERRED, Output, OutputGeometry, OutputId, OutputMode,
    OutputSubpixel, OutputTransform, cursor_bounds, output_contains,
};
pub use scene::{BufferTransform, Frame, Scene, SceneElement};
pub use shm_guard::patched_pages;
pub use texture::{PixelFormat, TextureId, TextureImage, TextureRect, TextureSource, UploadPixels};

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

/// A request from the compositor to the backend.
///
/// The counterpart to [`BackendMessage`], and deliberately not a call: the
/// compositor task blocks on nothing, so it asks and carries on, and the
/// answer arrives later as a `BackendMessage` like any other backend event.
/// Anything needing the GL context — which importing a dma-buf does — has to
/// go this way round, because that context belongs to the backend thread and
/// cannot be borrowed across.
#[derive(Debug)]
pub enum BackendRequest {
    /// Report which dma-buf formats can be imported, having checked that
    /// importing actually works. Answered with [`BackendMessage::DmabufSupport`].
    ///
    /// The backend may not be able to answer yet — a hosted backend has no GL
    /// context until its window exists — in which case it answers as soon as
    /// it can rather than refusing.
    ProbeDmabuf,
    /// Try importing one client buffer and report whether it took, answered
    /// with [`BackendMessage::DmabufImported`] carrying the same token.
    ///
    /// A client is waiting on this — `zwp_linux_buffer_params_v1.create`
    /// cannot say `created` or `failed` until the driver has actually tried —
    /// so it must always be answered, including by a backend that has no way
    /// to try.
    ImportDmabuf {
        /// Identifies this import. Never reused, so a late answer cannot be
        /// mistaken for the answer to a later question.
        token: u64,
        /// The buffer to try.
        image: std::sync::Arc<dmabuf::DmabufImage>,
    },
}

/// What produced a scroll.
///
/// A wheel moves in detents and stops between them; a touchpad moves smoothly
/// and has a definite end. Clients treat the two differently — kinetic
/// scrolling belongs to one and not the other — and cannot tell them apart from
/// the deltas alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSource {
    /// A mouse wheel, clicking through detents.
    Wheel,
    /// A touchpad or trackpoint, moving smoothly, with an end the user makes by
    /// lifting their fingers.
    Finger,
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
        /// A touchscreen is present and available
        touch: bool,
    },
    /// A message reporting output info
    OutputInfo {
        /// A vector of all available outputs
        outputs: Vec<output::Output>,
    },
    /// A message that the backend host has closed.  Only applicable to winit backend
    /// or other future backends that can be closed by an externality
    Closed,
    /// The backend is ready to show another frame on this output, and is
    /// asking for one.
    ///
    /// This is what paces rendering, and it comes from the backend because
    /// only the backend knows when a display can take a frame: a page flip has
    /// completed, or a host compositor has said now is the time to draw. A
    /// request stands until it is answered, so an output that asks while
    /// nothing has changed is served the moment something does.
    ///
    /// One output asking says nothing about any other. Two displays at
    /// different refresh rates ask at different times, and there is no rate the
    /// compositor could pick that would be right for both.
    FrameRequested(OutputId),
    /// A scene has reached the screen on this output.
    ///
    /// Carries the output because frame callbacks and presentation feedback
    /// belong to the surfaces that were shown, and on a second display those
    /// are different surfaces at a different moment.
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
    /// A finger has touched the screen.
    ///
    /// Touch is multi-point, so every event names which finger it is about.
    /// The id is the backend's, and is only required to be unique among the
    /// fingers currently down — one that has been lifted may be reused.
    TouchDown {
        /// Which finger.
        id: i32,
        /// Where it landed, in global compositor coordinates.
        x: f64,
        /// Likewise.
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
    /// The touch sequence has been taken over by something else — a gesture
    /// recogniser, or the compositor itself — and every point in it is void.
    ///
    /// Not the same as every finger lifting: a client that has been sent this
    /// must undo whatever the sequence was doing rather than complete it.
    TouchCancel,
    /// The pointer's scroll axes have moved.
    MouseScroll {
        /// The change in the x axis of the scroll
        dx: f64,
        /// The change in the y axis of the scroll
        dy: f64,
        /// What did the scrolling, which decides how a client should treat it.
        source: ScrollSource,
        /// Wheel detents on each axis, in 120ths of a click, and zero for a
        /// source that does not click. The unit is the protocol's: it lets a
        /// high-resolution wheel report a fraction of a detent without needing
        /// a different event from an ordinary one.
        v120_x: i32,
        v120_y: i32,
    },
    /// A continuous scroll has finished — the fingers have left the touchpad.
    ///
    /// Worth a message of its own because it is information a client cannot
    /// infer. Scrolling that merely pauses and scrolling that has ended look
    /// identical from a stream of deltas, and kinetic scrolling needs to tell
    /// them apart.
    MouseScrollEnd,
    /// What this backend can do with dma-bufs, in answer to
    /// [`BackendRequest::ProbeDmabuf`].
    DmabufSupport {
        /// Formats and modifiers that can be imported. Empty when there is no
        /// import path, which is what the compositor keys off: no formats
        /// means nothing to advertise to clients.
        formats: Vec<dmabuf::DmabufFormat>,
        /// What came of actually trying it.
        probe: dmabuf::DmabufProbe,
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
