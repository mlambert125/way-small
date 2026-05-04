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
