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
#[derive(Debug)]
pub struct Scene {
    /// The target output
    pub output_id: OutputId,
    /// Distinguishes this scene from the last one composed for the same
    /// output, and rises with every one.
    ///
    /// Outputs are paced apart — each is composed when the backend says it can
    /// show another frame for it — so a published frame is a mixture of scenes
    /// composed at different moments, most of which the backend has already
    /// drawn. This is what tells it which one it has not.
    pub serial: u64,
    /// The elements to draw
    pub elements: Vec<SceneElement>,
}

/// One frame: the newest scene for every output.
///
/// Not a moment in time — the scenes in it were composed at whatever moment
/// their own output last asked for one. It is a slot holding the latest state
/// of every output at once, so that publishing a new scene for one output
/// cannot drop an unshown scene belonging to another. The serial on each scene
/// is what a backend uses to tell what is new to it.
pub type Frame = Vec<Arc<Scene>>;
