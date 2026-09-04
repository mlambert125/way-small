//! What the user did, as a backend reports it.

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

/// What produced a scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSource {
    /// A mouse wheel, clicking through detents.
    Wheel,
    /// A touchpad or trackpoint, moving smoothly, with an end the user makes by lifting their fingers.
    Finger,
}
