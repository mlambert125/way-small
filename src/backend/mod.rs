//! Backend subsystem.
//!
//! A backend connects the compositor to a set of I/O devices: it displays the
//! compositor's output and turns host input into `BackendMessage`s. What passes
//! between a backend and the compositor is defined in [`crate::shared`],
//! which belongs to neither; this module is only the implementations.

/// Imports client GPU buffers through EGL, for the renderer to sample.
pub mod dmabuf_import;
/// Draws a `Scene` through GLES. Shared by every backend that displays
/// anything; only the way the GL context is obtained differs between them.
pub mod gl_renderer;
/// Headless backend: displays nothing, captures nothing.
pub mod null;
/// Hosted backend: a window on an existing compositor, for development.
pub mod winit;
