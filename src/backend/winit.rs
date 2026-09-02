//! Winit display backend.
//!
//! Opens a window on the host compositor via winit, draws the scenes it
//! receives from the compositor through GLES 3.0, and translates host input
//! events (keyboard, mouse, focus) into `BackendMessages` sent to the
//! compositor loop. Also manages XKB state for keycode-to-keysym resolution.
//!
//! The GL context is created and made current on the winit thread and never
//! leaves it, which is why compositing lives here rather than in the
//! compositor task. The same EGL display is what client GPU buffers are
//! imported through — see [`super::dmabuf`] — so a dma-buf a client hands over
//! is sampled by the same renderer, on the same thread, with no upload.

use super::dmabuf::DmabufImporter;
use super::gl_renderer::GlRenderer;
use crate::shared::{
    BackendMessage, BackendRequest, ButtonState, DmabufFormat, DmabufProbe, Frame, KeyState,
    MouseButton, PresentedAt, Scene, fourcc_name,
};
use crate::shared::{OUTPUT_MODE_CURRENT, OUTPUT_MODE_PREFERRED, Output, OutputId};
use glutin::config::{Config, ConfigTemplateBuilder, GlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext, Version,
};
use glutin::display::{AsRawDisplay, GetGlDisplay, GlDisplay, RawDisplay};
use glutin::surface::{GlSurface, Surface as GlSurfaceHandle, SwapInterval, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasWindowHandle;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    platform::scancode::PhysicalKeyExtScancode,
    platform::wayland::EventLoopBuilderExtWayland,
    window::{Window, WindowAttributes, WindowId},
};
use xkbcommon::xkb;

/// The one and only output id for this winit host
const WINIT_OUTPUT_ID: OutputId = OutputId(1);

/// Events coming from outside of the window that should be handled by the
/// winit event loop
enum UserEvent {
    /// A shutdown event happening from outside that should exit the event loop
    Shutdown,
    /// A new frame is in the slot. Carries nothing: the payload is read from
    /// the watch receiver at the point of drawing, so wake-ups that pile up
    /// behind a slow frame collapse into one draw of the newest frame instead
    /// of a backlog of stale ones.
    FrameReady,
    /// The compositor has asked the backend for something. Carried into the
    /// event loop rather than handled where it arrives, because answering it
    /// needs the GL context and that only exists on this thread.
    Request(BackendRequest),
}

/// The window and everything bound to its GL context.
///
/// Created together in `resumed` because none of it is useful without the
/// rest, and dropped together so the context outlives the renderer's textures.
struct GlState {
    /// What this driver can do with dma-bufs, worked out once when the context
    /// was made. Stored rather than re-derived because probing costs a texture
    /// round trip and the answer cannot change while the context lives.
    dmabuf_support: (Vec<DmabufFormat>, DmabufProbe),
    /// The winit window
    window: Arc<Window>,
    /// The window GL surface
    surface: GlSurfaceHandle<WindowSurface>,
    /// The Gl context
    context: PossiblyCurrentContext,
    /// The Gl renderer
    renderer: GlRenderer,
}

/// Winit application
struct App {
    /// Signals that the backend is up, which is what releases the rest of
    /// startup. Sent once the window and GL context exist and the backend has
    /// reported what it can do — not merely once the thread is running.
    ///
    /// Everything a client learns at connection time is decided by then: a
    /// socket advertised earlier would let a client enumerate the globals
    /// before dma-buf support is known, and it would pick shm for the rest of
    /// its life on the strength of that.
    ready: Option<tokio::sync::oneshot::Sender<()>>,
    /// GL State data
    gl: Option<GlState>,
    backend_sender: Sender<BackendMessage>,
    /// Cancellation token for this host to cancel the compositor at large
    /// when a window is closed, or an unrecoverable error occurs
    cancel_token: CancellationToken,
    /// xkb_context and xkb_keymap must outlive xkb_state
    #[allow(dead_code)]
    xkb_context: xkb::Context,
    /// XKB keyboard map for mapping winit keycodes to xkb keymaps
    #[allow(dead_code)]
    xkb_keymap: xkb::Keymap,
    /// Keyboard state
    xkb_state: xkb::State,
    /// The newest frame the compositor has published.
    frames: watch::Receiver<Frame>,
    /// Last drawn frame, for repainting on resize
    last_frame: Option<Frame>,
    /// The serial of the newest scene actually drawn on each output.
    ///
    /// A published frame carries the newest scene for every output, most of
    /// which have already been drawn — the compositor recomposes an output
    /// only when that output has asked. This is what separates the one scene
    /// that is new from the ones that are being carried along.
    drawn: HashMap<OutputId, u64>,
    /// A dma-buf probe that arrived before there was a GL context to answer it
    /// with. Answered from `resumed` instead of being refused: the compositor
    /// asks as it starts up, and a hosted backend has no context until its
    /// window exists.
    dmabuf_probe_pending: bool,
}

impl App {
    /// Draw a scene and put it on screen.
    ///
    /// The drawable size is read back from the window rather than taken from
    /// the scene: a resize reaches winit before the compositor has produced a
    /// scene at the new size, and drawing the old scene into the new viewport
    /// is better than skipping the frame.
    fn present_scene(&mut self, scene: &Scene) {
        if scene.output_id != WINIT_OUTPUT_ID {
            return;
        }
        self.drawn.insert(scene.output_id, scene.serial);
        let Some(gl) = self.gl.as_mut() else {
            return;
        };
        let size = gl.window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };

        gl.surface.resize(&gl.context, width, height);
        gl.renderer.draw(scene, size.width, size.height);
        if let Err(e) = gl.surface.swap_buffers(&gl.context) {
            warn!("failed to swap buffers: {e}");
        }
    }

    /// Draw whatever the compositor has published since the last draw.
    ///
    /// Several wake-ups can arrive for one frame, or one wake-up can cover
    /// several frames, so the receiver's own change flag decides whether there
    /// is anything to do rather than the number of events.
    fn present_pending_frames(&mut self) {
        if !self.frames.has_changed().unwrap_or(false) {
            return;
        }
        let frames = self.frames.borrow_and_update().clone();
        let mut presented = Vec::new();
        for scene in &frames {
            // Outputs are paced apart, so a frame is mostly scenes already on
            // screen. Redrawing one costs a swap for no change, and would have
            // this output report a presentation it did not make — which is
            // what fires its clients' frame callbacks.
            if self.drawn.get(&scene.output_id) == Some(&scene.serial) {
                continue;
            }
            self.present_scene(scene);
            presented.push(scene.output_id);
        }
        if let Some(gl) = self.gl.as_mut() {
            gl.renderer.drop_unused_cached_textures(&frames);
        }
        self.last_frame = Some(frames);

        for output_id in presented {
            // Reported even if there was no context to draw with or the window
            // had no area. The scene is dealt with either way, and a backend
            // that went quiet here would strand every client waiting on a
            // frame callback.
            let _ = self
                .backend_sender
                .try_send(BackendMessage::FramePresented(output_id, PresentedAt::now()));
            // And the pacing: this output can take another frame once the host
            // says it is time to draw again. Asking only after presenting is
            // what bounds the compositor to one frame in flight per output.
            self.ask_for_a_frame();
        }
    }

    /// Ask the host when to draw next, and pass that on as a frame request.
    ///
    /// A hosted backend has no vblank of its own — it is a client of another
    /// compositor, and `RedrawRequested` is that compositor telling it when a
    /// frame it draws will be shown. That is the same signal a page flip
    /// completing will be on a DRM backend, arriving by a different route.
    fn ask_for_a_frame(&mut self) {
        if let Some(gl) = self.gl.as_ref() {
            gl.window.request_redraw();
        }
    }

    /// Redraw the last frame, for when the window changed but the scene did not.
    fn repaint(&mut self) {
        let Some(frame) = self.last_frame.clone() else {
            return;
        };
        for scene in &frame {
            self.present_scene(scene);
        }
    }

    /// Create the window, EGL context, and renderer.
    fn init_gl(event_loop: &ActiveEventLoop) -> anyhow::Result<GlState> {
        let window_attributes = WindowAttributes::default().with_title("way-small");
        let (window, config) = DisplayBuilder::new()
            .with_window_attributes(Some(window_attributes))
            .build(event_loop, ConfigTemplateBuilder::new(), pick_config)
            .map_err(|e| anyhow::anyhow!("failed to create GL display: {e}"))?;
        let window = Arc::new(
            window.ok_or_else(|| anyhow::anyhow!("GL display builder returned no window"))?,
        );

        let display = config.display();
        // GLES rather than desktop GL: it is what the shaders target, and what
        // is universally available on the Mesa drivers a compositor runs on.
        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(Some(Version::new(3, 0))))
            .build(Some(window.window_handle()?.as_raw()));
        // SAFETY: the window outlives the context — both live in `GlState`,
        // and `window` is declared first so it is dropped last.
        let not_current = unsafe { display.create_context(&config, &context_attributes)? };

        let surface_attributes = window.build_surface_attributes(<_>::default())?;
        // SAFETY: the attributes carry this window's handle, and the window
        // outlives the surface for the same reason as the context.
        let surface = unsafe { display.create_window_surface(&config, &surface_attributes)? };
        let context = not_current.make_current(&surface)?;
        // Only one variant exists: glutin is built here with the EGL backend
        // alone, which is also the only one a dma-buf can be imported through.
        let RawDisplay::Egl(raw_display) = display.raw_display();

        // Pacing comes from the host, through `RedrawRequested`, and a frame is
        // only composed once this backend has asked for one. Blocking the swap
        // on vblank as well would pace nothing extra and would stall the
        // thread that handles input while it waited.
        if let Err(e) = surface.set_swap_interval(&context, SwapInterval::DontWait) {
            warn!("failed to disable vsync: {e}");
        }

        // SAFETY: the display is the one the context above was made on, and it
        // stays current on this thread for as long as the importer is used.
        let importer =
            unsafe { DmabufImporter::new(raw_display, &|symbol| display.get_proc_address(symbol)) };
        let importer = match importer {
            Ok(importer) => Some(importer),
            Err(reason) => {
                info!("dma-buf import unavailable: {reason}");
                None
            }
        };

        // SAFETY: the context was just made current on this thread and stays
        // current for as long as the renderer lives.
        let renderer =
            unsafe { GlRenderer::new(|symbol| display.get_proc_address(symbol), importer)? };

        // Ask the driver what it takes, and check that it means it. Done here,
        // once, because it needs the context and nothing about the answer can
        // change while that context lives.
        let dmabuf_support = renderer.dmabuf_support();
        report_dmabuf_support(&dmabuf_support);

        Ok(GlState {
            dmabuf_support,
            window,
            surface,
            context,
            renderer,
        })
    }

    /// Try importing one client buffer and report back.
    ///
    /// Answered even when there is no context to try with: a client's `create`
    /// is waiting on this, and silence would hang it.
    fn answer_dmabuf_import(&mut self, token: u64, image: &crate::shared::DmabufImage) {
        let imported = self
            .gl
            .as_ref()
            .is_some_and(|gl| gl.renderer.verify_import(image));
        let _ = self
            .backend_sender
            .blocking_send(BackendMessage::DmabufImportResult { token, imported });
    }

    /// Answer a dma-buf probe, or remember to once there is a context.
    fn answer_dmabuf_probe(&mut self) {
        let Some(gl) = self.gl.as_ref() else {
            self.dmabuf_probe_pending = true;
            return;
        };
        self.dmabuf_probe_pending = false;
        let (formats, probe) = &gl.dmabuf_support;
        let _ = self
            .backend_sender
            .blocking_send(BackendMessage::DmabufSupport {
                formats: formats.clone(),
                probe: probe.clone(),
            });
    }
}

/// Log what the driver said about dma-buf, at the level the answer deserves.
///
/// A failed probe is a warning rather than a debug line: the extensions are
/// there, so a client will be told dma-buf works, and it will not.
fn report_dmabuf_support(support: &(Vec<DmabufFormat>, DmabufProbe)) {
    let (formats, probe) = support;
    match probe {
        DmabufProbe::Passed => info!(
            "dma-buf import working: {} format(s), e.g. {}",
            formats.len(),
            formats
                .iter()
                .take(4)
                .map(|f| fourcc_name(f.fourcc))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        DmabufProbe::Unsupported(reason) => info!("dma-buf import unavailable: {reason}"),
        DmabufProbe::Untested(reason) => info!(
            "dma-buf import available for {} format(s) but unverified: {reason}",
            formats.len(),
        ),
        DmabufProbe::Failed(reason) => {
            warn!("dma-buf import is advertised by the driver but does not work: {reason}");
        }
    }
}

/// Prefer the config with the fewest samples: this compositor draws axis-aligned
/// quads, so multisampling would cost bandwidth and change nothing.
fn pick_config(configs: Box<dyn Iterator<Item = Config> + '_>) -> Config {
    configs
        .reduce(|best, config| {
            if config.num_samples() < best.num_samples() {
                config
            } else {
                best
            }
        })
        .expect("no GL config available")
}

impl ApplicationHandler<UserEvent> for App {
    /// Called once when the app starts and is ready.  This is a bit poorly
    /// named for platforms that don't do tombstoning (desktops), but that's
    /// what winit::application::ApplicationHandler calls it
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gl.is_some() {
            return;
        }

        let gl = match Self::init_gl(event_loop) {
            Ok(gl) => gl,
            Err(e) => {
                // Without a context there is nothing to display, and no
                // software path to fall back to, so stop rather than run blind.
                error!("failed to initialise GL backend: {e:#}");
                // Release startup even so: it is waiting on this, and a
                // compositor that is shutting down should not also hang.
                if let Some(ready) = self.ready.take() {
                    let _ = ready.send(());
                }
                let _ = self.backend_sender.blocking_send(BackendMessage::Closed);
                self.cancel_token.cancel();
                event_loop.exit();
                return;
            }
        };

        // Report hardware capabilities
        let size = gl.window.inner_size();
        let pending_probe = self.dmabuf_probe_pending;
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
                    geometry: crate::shared::OutputGeometry {
                        x: 0,
                        y: 0,
                        physical_width: size.width.cast_signed(),
                        physical_height: size.height.cast_signed(),
                        subpixel: crate::shared::OutputSubpixel::None,
                        make: String::from("winit"),
                        model: String::from("winit"),
                        transform: crate::shared::OutputTransform::Normal,
                    },
                    modes: vec![crate::shared::OutputMode {
                        flags: OUTPUT_MODE_CURRENT | OUTPUT_MODE_PREFERRED,
                        width: size.width.cast_signed(),
                        height: size.height.cast_signed(),
                        refresh_mhz: 60000,
                    }],
                    scale: 1,
                }],
            });

        gl.window.set_cursor_visible(false);
        self.gl = Some(gl);

        // A probe that arrived before there was a context to answer it with.
        if pending_probe {
            self.answer_dmabuf_probe();
        }

        // Everything a connecting client will be told now exists.
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(());
        }

        // Clear to the background so the window is not showing whatever was in
        // the buffer before the first scene arrives.
        let empty = Scene {
            output_id: WINIT_OUTPUT_ID,
            serial: 0,
            elements: Vec::new(),
        };
        self.present_scene(&empty);
        // Nothing has been composed for this output yet, and nothing will be
        // until it is asked for. This is the first turn of that loop.
        let _ = self
            .backend_sender
            .try_send(BackendMessage::FrameRequested(WINIT_OUTPUT_ID));
    }

    /// Window event handler (called by winit event loop)
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
                    size.width.cast_signed(),
                    size.height.cast_signed(),
                ));
                self.repaint();
            }
            WindowEvent::RedrawRequested => {
                // The host is ready to show another frame. Repaint what is
                // already here so the window is never blank, and tell the
                // compositor it may compose the next one.
                self.repaint();
                let _ = self
                    .backend_sender
                    .try_send(BackendMessage::FrameRequested(WINIT_OUTPUT_ID));
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
                    .blocking_send(BackendMessage::MouseMovedTo {
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
                    winit::event::MouseScrollDelta::LineDelta(x, y) => (f64::from(x), f64::from(y)),
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

    /// User event handler (called by winit event loop)
    /// Winit separates this handler from the normal `windows_event` handler above
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Shutdown => {
                event_loop.exit();
            }
            UserEvent::FrameReady => self.present_pending_frames(),
            UserEvent::Request(BackendRequest::ProbeDmabuf) => self.answer_dmabuf_probe(),
            UserEvent::Request(BackendRequest::ImportDmabuf { token, image }) => {
                self.answer_dmabuf_import(token, &image);
            }
        }
    }
}

/// Runs this wayland backend, waiting for frames from the compositor and
/// sending over input events from keyboard/mouse, etc.
pub fn run_winit_backend(
    backend_sender: Sender<BackendMessage>,
    cancel_token: &CancellationToken,
    ready_tx: tokio::sync::oneshot::Sender<()>,
    frame_rx: watch::Receiver<Frame>,
    mut requests: Receiver<BackendRequest>,
) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .with_any_thread(true)
        .build()?;

    let proxy: EventLoopProxy<UserEvent> = event_loop.create_proxy();
    let rt = tokio::runtime::Handle::current();

    let shutdown_proxy = proxy.clone();
    let cancel_token_for_shutdown = cancel_token.clone();
    rt.spawn(async move {
        cancel_token_for_shutdown.cancelled().await;
        let _ = shutdown_proxy.send_event(UserEvent::Shutdown);
    });

    // Only nudges the event loop; the frame itself stays in the slot until the
    // winit thread is ready to draw it.
    let frame_proxy = proxy.clone();
    let mut notify_rx = frame_rx.clone();
    rt.spawn(async move {
        while notify_rx.changed().await.is_ok() {
            if frame_proxy.send_event(UserEvent::FrameReady).is_err() {
                break;
            }
        }
    });

    // Requests are handled on the winit thread because answering them needs
    // the GL context, so they come in as user events rather than being read
    // where they arrive.
    let request_proxy = proxy.clone();
    rt.spawn(async move {
        while let Some(request) = requests.recv().await {
            if request_proxy
                .send_event(UserEvent::Request(request))
                .is_err()
            {
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
        ready: Some(ready_tx),
        gl: None,
        backend_sender,
        cancel_token: cancel_token.clone(),
        xkb_context,
        xkb_keymap,
        xkb_state,
        frames: frame_rx,
        last_frame: None,
        drawn: HashMap::new(),
        dmabuf_probe_pending: false,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
