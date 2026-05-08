//! Backend message types.
//!
//! Defines BackendMessage (backend→compositor) for input events and lifecycle,
//! and RenderFrame (compositor→backend) for presenting composited output.

pub const BACKGROUND_COLOR: u32 = 0xff1a_1a2e;

pub struct RenderFrame {
    pub pixels: Vec<u32>,
    pub width: u32,
    pub height: u32,
}

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
    SeatCapabilities {
        pointer: bool,
        keyboard: bool,
    },
    OutputInfo {
        width: u32,
        height: u32,
        refresh_mhz: u32,
    },
    Closed,
    Resized(u32, u32),
    KeyInput {
        keycode: u32,
        state: KeyState,
        mods_depressed: u32,
        mods_latched: u32,
        mods_locked: u32,
        mods_group: u32,
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
