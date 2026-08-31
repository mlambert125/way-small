//! What to draw, for one output and for one frame.

use super::output::OutputId;
use super::texture::TextureImage;
use std::sync::Arc;

/// One textured quad, in output pixel coordinates.
#[derive(Debug, Clone)]
pub struct SceneElement {
    pub texture: Arc<TextureImage>,
    /// Source rectangle in texture pixels: (x, y, width, height).
    pub src: (f64, f64, f64, f64),
    /// Destination rectangle in output pixels: (x, y, width, height).
    pub dst: (i32, i32, i32, i32),
}

/// Everything to draw for one output, back to front.
///
/// Carries no size of its own: the quads are in the output's pixel
/// coordinates, and how big the drawable actually is right now is known only
/// to the backend that owns it. During a resize the two disagree for a frame
/// or two, and the backend's answer is the correct one.
#[derive(Debug)]
pub struct Scene {
    /// The target output
    pub output_id: OutputId,
    /// The elements to draw
    pub elements: Vec<SceneElement>,
}

/// One frame: the scene for every output, as of a single compositor tick.
///
/// Outputs travel together because a frame is a moment in time rather than a
/// per-output event, and because only a whole frame can be meaningfully
/// superseded by a newer one.
pub type Frame = Vec<Arc<Scene>>;
