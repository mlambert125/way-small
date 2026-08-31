//! Backend subsystem.
//!
//! A backend connects the compositor to a set of I/O devices: it displays the
//! compositor's output and turns host input into `BackendMessage`s. What passes
//! between a backend and the compositor is defined in [`crate::shared`],
//! which belongs to neither; this module is only the implementations.
//!
//! The compositor never rasterises. It builds a `Scene` — a back-to-front list
//! of textured quads — and a backend turns that into GPU work inside its own
//! GL context, through the shared [`gl_renderer`]. Rasterising in the
//! compositor task is not an option: a GL context is bound to one thread, and
//! that thread is the backend's.

/// Draws a `Scene` through GLES. Shared by every backend that displays
/// anything; only the way the GL context is obtained differs between them.
pub mod gl_renderer;
/// Headless backend: displays nothing, captures nothing.
pub mod null;
/// Hosted backend: a window on an existing compositor, for development.
pub mod winit;
