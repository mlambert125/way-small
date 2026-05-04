use std::num::NonZeroU32;
use std::sync::Arc;

use softbuffer::{Context, Surface};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    platform::wayland::EventLoopBuilderExtWayland,
    window::{Window, WindowAttributes, WindowId},
};
use xkbcommon::xkb;

use crate::backend::{BackendMessage, ButtonState, KeyState, MouseButton};

#[derive(Debug)]
enum UserEvent {
    Shutdown,
}

struct App {
    // Kept alive to satisfy the borrow required by Surface
    context: Option<Context<Arc<Window>>>,
    window: Option<Arc<Window>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    backend_sender: Sender<BackendMessage>,
    cancel_token: CancellationToken,
    // xkb_context and xkb_keymap must outlive xkb_state
    #[allow(dead_code)]
    xkb_context: xkb::Context,
    #[allow(dead_code)]
    xkb_keymap: xkb::Keymap,
    xkb_state: xkb::State,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let window = Arc::new(
                event_loop
                    .create_window(WindowAttributes::default().with_title("way-small"))
                    .expect("failed to create window"),
            );

            let context =
                Context::new(window.clone()).expect("failed to create softbuffer context");
            let surface = Surface::new(&context, window.clone()).expect("failed to create surface");

            self.context = Some(context);
            self.surface = Some(surface);
            window.request_redraw();
            self.window = Some(window);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                let _ = self.backend_sender.blocking_send(BackendMessage::Closed);
                self.cancel_token.cancel();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                let _ = self.backend_sender.blocking_send(BackendMessage::Resized(size.width, size.height));
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut())
                {
                    let size = window.inner_size();
                    if let (Some(width), Some(height)) =
                        (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                    {
                        surface
                            .resize(width, height)
                            .expect("failed to resize surface");
                        let mut buffer =
                            surface.buffer_mut().expect("failed to get surface buffer");
                        buffer.fill(0xff1a_1a2e);
                        buffer.present().expect("failed to present buffer");
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let winit::keyboard::PhysicalKey::Code(code) = event.physical_key {
                    // winit KeyCode enum → evdev scancode + 8 = xkb keycode
                    let xkb_keycode: xkb::Keycode = (code as u32 + 8).into();
                    let key_state = if event.state.is_pressed() {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    };
                    let keysym = self.xkb_state.key_get_one_sym(xkb_keycode);
                    let direction = if event.state.is_pressed() {
                        xkb::KeyDirection::Down
                    } else {
                        xkb::KeyDirection::Up
                    };
                    self.xkb_state.update_key(xkb_keycode, direction);
                    let _ = self.backend_sender.blocking_send(BackendMessage::KeyInput {
                        keycode: xkb_keycode.raw(),
                        keysym: keysym.raw(),
                        state: key_state,
                    });
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let _ = self.backend_sender.blocking_send(BackendMessage::MouseMove {
                    x: position.x,
                    y: position.y,
                });
            }
            WindowEvent::MouseInput { button, state, .. } => {
                let btn = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    _ => return,
                };
                let st = if state.is_pressed() {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                let _ = self.backend_sender.blocking_send(BackendMessage::MouseButton {
                    button: btn,
                    state: st,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => (x as f64, y as f64),
                    winit::event::MouseScrollDelta::PixelDelta(pos) => (pos.x, pos.y),
                };
                let _ = self.backend_sender.blocking_send(BackendMessage::MouseScroll { dx, dy });
            }
            WindowEvent::Focused(focused) => {
                let msg = if focused {
                    BackendMessage::FocusIn
                } else {
                    BackendMessage::FocusOut
                };
                let _ = self.backend_sender.blocking_send(msg);
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Shutdown => {
                event_loop.exit();
            }
        }
    }
}

pub fn run_winit_backend(
    backend_sender: Sender<BackendMessage>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .with_any_thread(true)
        .build()?;

    let proxy: EventLoopProxy<UserEvent> = event_loop.create_proxy();
    let rt = tokio::runtime::Handle::current();

    let cancel_token_for_shutdown = cancel_token.clone();
    rt.spawn(async move {
        cancel_token_for_shutdown.cancelled().await;
        let _ = proxy.send_event(UserEvent::Shutdown);
    });

    let xkb_context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let xkb_keymap = xkb::Keymap::new_from_names(
        &xkb_context,
        "",
        "",
        "",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("failed to create xkb keymap");
    let xkb_state = xkb::State::new(&xkb_keymap);

    let mut app = App {
        context: None,
        window: None,
        surface: None,
        backend_sender,
        cancel_token: cancel_token.clone(),
        xkb_context,
        xkb_keymap,
        xkb_state,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
