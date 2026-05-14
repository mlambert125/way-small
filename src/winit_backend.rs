//! Winit display backend.
//!
//! Opens a window on the host compositor via winit, renders composited frames
//! received from the compositor, and translates host input events (keyboard,
//! mouse, focus) into BackendMessages sent to the compositor loop. Also manages
//! XKB state for keycode-to-keysym resolution.

use std::num::NonZeroU32;
use std::sync::Arc;

use softbuffer::{Context, Surface};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    platform::scancode::PhysicalKeyExtScancode,
    platform::wayland::EventLoopBuilderExtWayland,
    window::{Window, WindowAttributes, WindowId},
};
use xkbcommon::xkb;

use crate::backend::{
    BACKGROUND_COLOR, BackendMessage, ButtonState, KeyState, MouseButton, RenderFrame,
};

use crate::protocol::state::{Output, OutputId, OUTPUT_MODE_CURRENT, OUTPUT_MODE_PREFERRED};

const WINIT_OUTPUT_ID: OutputId = OutputId(1);

enum UserEvent {
    Shutdown,
    Frame(Arc<RenderFrame>),
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
    // Last received frame for repainting on resize
    last_frame: Option<Arc<RenderFrame>>,
}

impl App {
    fn present_frame(&mut self, frame: &RenderFrame) {
        if frame.output_id != WINIT_OUTPUT_ID {
            return;
        }
        if let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) {
            let size = window.inner_size();
            if let (Some(width), Some(height)) =
                (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
            {
                surface
                    .resize(width, height)
                    .expect("failed to resize surface");
                let mut buffer = surface.buffer_mut().expect("failed to get surface buffer");

                // Blit the frame into the window buffer, handling size mismatch
                let dst_w = size.width as usize;
                let dst_h = size.height as usize;
                let src_w = frame.width as usize;
                let src_h = frame.height as usize;

                for y in 0..dst_h.min(src_h) {
                    let dst_start = y * dst_w;
                    let src_start = y * src_w;
                    let copy_w = dst_w.min(src_w);
                    buffer[dst_start..dst_start + copy_w]
                        .copy_from_slice(&frame.pixels[src_start..src_start + copy_w]);
                    // Fill remaining columns with background
                    if dst_w > src_w {
                        for x in src_w..dst_w {
                            buffer[dst_start + x] = BACKGROUND_COLOR;
                        }
                    }
                }
                // Fill remaining rows with background
                for y in src_h..dst_h {
                    let dst_start = y * dst_w;
                    for x in 0..dst_w {
                        buffer[dst_start + x] = BACKGROUND_COLOR;
                    }
                }

                buffer.present().expect("failed to present buffer");
            }
        }
    }
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

            // Report hardware capabilities
            let size = window.inner_size();
            let _ = self
                .backend_sender
                .blocking_send(BackendMessage::SeatCapabilities {
                    pointer: true,
                    keyboard: true,
                });
            let _ = self
                .backend_sender
                .blocking_send(BackendMessage::OutputInfo {
                    outputs: vec![Output {
                        id: WINIT_OUTPUT_ID,
                        name: String::from("winit"),
                        description: String::from("winit display backend"),
                        geometry: crate::protocol::state::OutputGeometry {
                            x: 0,
                            y: 0,
                            physical_width: size.width as i32,
                            physical_height: size.height as i32,
                            subpixel: crate::protocol::state::OutputSubpixel::None,
                            make: String::from("winit"),
                            model: String::from("winit"),
                            transform: crate::protocol::state::OutputTransform::Normal,
                        },
                        modes: vec![crate::protocol::state::OutputMode {
                            flags: OUTPUT_MODE_CURRENT | OUTPUT_MODE_PREFERRED,
                            width: size.width as i32,
                            height: size.height as i32,
                            refresh_mhz: 60000,
                        }],
                        scale: 1,
                    }],
                });

            window.set_cursor_visible(false);
            self.window = Some(window);

            let initial = RenderFrame {
                output_id: WINIT_OUTPUT_ID,
                pixels: vec![BACKGROUND_COLOR; (size.width * size.height) as usize],
                width: size.width as i32,
                height: size.height as i32,
            };
            self.present_frame(&initial);
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
                let _ = self.backend_sender.blocking_send(BackendMessage::Resized(
                    WINIT_OUTPUT_ID,
                    size.width as i32,
                    size.height as i32,
                ));
                if let Some(frame) = self.last_frame.clone() {
                    self.present_frame(&frame);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(frame) = self.last_frame.clone() {
                    self.present_frame(&frame);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(scancode) = event.physical_key.to_scancode() {
                    let xkb_keycode: xkb::Keycode = (scancode + 8).into();
                    let key_state = if event.state.is_pressed() {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    };
                    let _keysym = self.xkb_state.key_get_one_sym(xkb_keycode);
                    let direction = if event.state.is_pressed() {
                        xkb::KeyDirection::Down
                    } else {
                        xkb::KeyDirection::Up
                    };
                    self.xkb_state.update_key(xkb_keycode, direction);
                    let mods_depressed = self.xkb_state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
                    let mods_latched = self.xkb_state.serialize_mods(xkb::STATE_MODS_LATCHED);
                    let mods_locked = self.xkb_state.serialize_mods(xkb::STATE_MODS_LOCKED);
                    let mods_group = self.xkb_state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
                    let _ = self.backend_sender.blocking_send(BackendMessage::KeyInput {
                        keycode: xkb_keycode.raw(),
                        state: key_state,
                        mods_depressed,
                        mods_latched,
                        mods_locked,
                        mods_group,
                    });
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let _ = self
                    .backend_sender
                    .blocking_send(BackendMessage::MouseMove {
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
                let _ = self
                    .backend_sender
                    .blocking_send(BackendMessage::MouseButton {
                        button: btn,
                        state: st,
                    });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => (x as f64, y as f64),
                    winit::event::MouseScrollDelta::PixelDelta(pos) => (pos.x, pos.y),
                };
                let _ = self
                    .backend_sender
                    .blocking_send(BackendMessage::MouseScroll { dx, dy });
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
            UserEvent::Frame(frame) => {
                self.present_frame(&frame);
                self.last_frame = Some(frame);
            }
        }
    }
}

pub fn run_winit_backend(
    backend_sender: Sender<BackendMessage>,
    cancel_token: CancellationToken,
    ready_tx: tokio::sync::oneshot::Sender<()>,
    frame_rx: Receiver<Arc<RenderFrame>>,
) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .with_any_thread(true)
        .build()?;

    let _ = ready_tx.send(());

    let proxy: EventLoopProxy<UserEvent> = event_loop.create_proxy();
    let rt = tokio::runtime::Handle::current();

    let shutdown_proxy = proxy.clone();
    let cancel_token_for_shutdown = cancel_token.clone();
    rt.spawn(async move {
        cancel_token_for_shutdown.cancelled().await;
        let _ = shutdown_proxy.send_event(UserEvent::Shutdown);
    });

    let frame_proxy = proxy.clone();
    rt.spawn(async move {
        let mut frame_rx = frame_rx;
        while let Some(frame) = frame_rx.recv().await {
            if frame_proxy.send_event(UserEvent::Frame(frame)).is_err() {
                break;
            }
        }
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
        last_frame: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
