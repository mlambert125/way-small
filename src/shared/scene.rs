//! What to draw, for one output and for one frame.

use super::output::OutputId;
use super::texture::TextureImage;
use std::sync::Arc;

/// Background color for compositor
pub const BACKGROUND_COLOR: u32 = 0xff1a_1a2e;

/// How a client has already transformed its buffer, from `wl_surface.set_buffer_transform`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BufferTransform {
    #[default]
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    FlippedRotate90,
    FlippedRotate180,
    FlippedRotate270,
}

impl BufferTransform {
    /// From the `wl_output.transform` value a client sends, or `None` if it is invalid
    pub fn from_wire(value: u32) -> Option<Self> {
        Some(match value {
            0 => Self::Normal,
            1 => Self::Rotate90,
            2 => Self::Rotate180,
            3 => Self::Rotate270,
            4 => Self::Flipped,
            5 => Self::FlippedRotate90,
            6 => Self::FlippedRotate180,
            7 => Self::FlippedRotate270,
            _ => return None,
        })
    }

    /// Whether this transform exchanges the buffer's width and height.
    pub fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Rotate90 | Self::Rotate270 | Self::FlippedRotate90 | Self::FlippedRotate270
        )
    }

    /// Where a point on the surface reads from in the buffer, as an affine map
    /// over unit coordinates: `(origin, basis)` such that
    /// `source = origin + basis * destination`.
    ///
    /// `basis` is column-major, matching how GL reads a `mat2`: `basis[0]`
    /// scales the destination's x and `basis[1]` its y. Writing it row-major
    /// transposes the two quarter turns and leaves the symmetric transforms
    /// looking correct, which is a bug that hides well.
    ///
    /// This is the *inverse* of what the client did, because the client's value
    /// describes the transform it already applied and the compositor's job is
    /// to undo it. The flipped variants are a mirror about the vertical axis
    /// followed by the rotation, so their inverses are the rotation's inverse
    /// followed by the mirror — which is where the axis swaps come from.
    pub fn uv_map(self) -> ((f32, f32), [[f32; 2]; 2]) {
        match self {
            Self::Normal => ((0.0, 0.0), [[1.0, 0.0], [0.0, 1.0]]),
            Self::Rotate90 => ((1.0, 0.0), [[0.0, 1.0], [-1.0, 0.0]]),
            Self::Rotate180 => ((1.0, 1.0), [[-1.0, 0.0], [0.0, -1.0]]),
            Self::Rotate270 => ((0.0, 1.0), [[0.0, -1.0], [1.0, 0.0]]),
            Self::Flipped => ((1.0, 0.0), [[-1.0, 0.0], [0.0, 1.0]]),
            Self::FlippedRotate90 => ((0.0, 0.0), [[0.0, 1.0], [1.0, 0.0]]),
            Self::FlippedRotate180 => ((0.0, 1.0), [[1.0, 0.0], [0.0, -1.0]]),
            Self::FlippedRotate270 => ((1.0, 1.0), [[0.0, -1.0], [-1.0, 0.0]]),
        }
    }
}

/// One textured quad, in output pixel coordinates.
#[derive(Debug, Clone)]
pub struct SceneElement {
    pub texture: Arc<TextureImage>,
    /// Source rectangle in texture pixels: (x, y, width, height).
    pub src: (f64, f64, f64, f64),
    /// Destination rectangle in output pixels: (x, y, width, height).
    pub dst: (i32, i32, i32, i32),
    /// How the client transformed its buffer, which the draw has to undo.
    pub transform: BufferTransform,
    /// The client has promised this quad is fully opaque, so the blend can be
    /// skipped. A promise, not a measurement — see `Surface::opaque_region`.
    pub opaque: bool,
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

#[cfg(test)]
// Every coordinate here is 0.0 or 1.0, so the casts to `i32` are exact.
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::BufferTransform;

    /// Where a destination corner reads from in the buffer, under a transform.
    fn sample(transform: BufferTransform, dx: f32, dy: f32) -> (f32, f32) {
        let ((ox, oy), basis) = transform.uv_map();
        (
            ox + basis[0][0] * dx + basis[1][0] * dy,
            oy + basis[0][1] * dx + basis[1][1] * dy,
        )
    }

    #[test]
    fn an_untransformed_buffer_is_sampled_straight_through() {
        assert_eq!(sample(BufferTransform::Normal, 0.0, 0.0), (0.0, 0.0));
        assert_eq!(sample(BufferTransform::Normal, 1.0, 1.0), (1.0, 1.0));
    }

    #[test]
    fn every_transform_maps_the_quad_onto_itself() {
        // Whatever the rotation or flip, the four destination corners must land
        // on the four buffer corners — exactly once each. A map that did not
        // would be sampling outside the buffer or reading part of it twice.
        for transform in [
            BufferTransform::Normal,
            BufferTransform::Rotate90,
            BufferTransform::Rotate180,
            BufferTransform::Rotate270,
            BufferTransform::Flipped,
            BufferTransform::FlippedRotate90,
            BufferTransform::FlippedRotate180,
            BufferTransform::FlippedRotate270,
        ] {
            let mut corners: Vec<(i32, i32)> = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)]
                .into_iter()
                .map(|(dx, dy)| {
                    let (sx, sy) = sample(transform, dx, dy);
                    // Exact in binary: every value here is 0 or 1.
                    (sx.round() as i32, sy.round() as i32)
                })
                .collect();
            corners.sort_unstable();
            assert_eq!(
                corners,
                vec![(0, 0), (0, 1), (1, 0), (1, 1)],
                "{transform:?} does not cover the buffer exactly once"
            );
        }
    }

    #[test]
    fn a_quarter_turn_is_the_inverse_of_the_client_rotation() {
        // The client rotated its buffer 90 degrees counter-clockwise, so the
        // top-left of the surface reads from the bottom-left of the buffer.
        assert_eq!(sample(BufferTransform::Rotate90, 0.0, 0.0), (1.0, 0.0));
        assert_eq!(sample(BufferTransform::Rotate90, 1.0, 0.0), (1.0, 1.0));
    }

    #[test]
    fn only_the_quarter_turns_exchange_the_axes() {
        assert!(!BufferTransform::Normal.swaps_axes());
        assert!(!BufferTransform::Rotate180.swaps_axes());
        assert!(!BufferTransform::Flipped.swaps_axes());
        assert!(!BufferTransform::FlippedRotate180.swaps_axes());
        assert!(BufferTransform::Rotate90.swaps_axes());
        assert!(BufferTransform::Rotate270.swaps_axes());
        assert!(BufferTransform::FlippedRotate90.swaps_axes());
        assert!(BufferTransform::FlippedRotate270.swaps_axes());
    }

    #[test]
    fn a_transform_the_protocol_does_not_define_is_refused() {
        assert_eq!(BufferTransform::from_wire(0), Some(BufferTransform::Normal));
        assert_eq!(
            BufferTransform::from_wire(7),
            Some(BufferTransform::FlippedRotate270)
        );
        assert_eq!(BufferTransform::from_wire(8), None);
    }
}
