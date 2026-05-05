//! Backend message types.
//!
//! Defines BackendMessage (backend→compositor) for input events and lifecycle,
//! and RenderFrame (compositor→backend) for presenting composited output.

#[derive(Debug, Clone, Copy)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy)]
pub enum ButtonState {
    Pressed,
    Released,
}

#[derive(Debug, Clone, Copy)]
pub enum KeyState {
    Pressed,
    Released,
}

#[derive(Debug)]
pub enum BackendMessage {
    /// Backend has a seat with these input capabilities.
    SeatCapabilities {
        pointer: bool,
        keyboard: bool,
    },
    /// Backend has an output (monitor/display) with these properties.
    OutputInfo {
        width: u32,
        height: u32,
        refresh_mhz: u32, // refresh rate in millihertz (e.g. 60000 = 60Hz)
    },
    Closed,
    Resized(u32, u32),
    KeyInput {
        keycode: u32,
        keysym: u32,
        state: KeyState,
    },
    MouseMove {
        x: f64,
        y: f64,
    },
    MouseButton {
        button: MouseButton,
        state: ButtonState,
    },
    MouseScroll {
        dx: f64,
        dy: f64,
    },
    FocusIn,
    FocusOut,
}

/// A composited frame ready for the backend to present.
pub struct RenderFrame {
    pub pixels: Vec<u32>,
    pub width: u32,
    pub height: u32,
}
