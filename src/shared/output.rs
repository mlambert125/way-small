//! How a backend describes the displays it has, and the compositor describes
//! them onward to clients as `wl_output`.

use strum::FromRepr;

/// The output mode is the currently selected mode
pub const OUTPUT_MODE_CURRENT: u32 = 0x1;
/// The output mode is the preferred output mode for this output
pub const OUTPUT_MODE_PREFERRED: u32 = 0x2;

/// Transform on this output (flipped/rotated)
#[derive(Debug, Clone, Copy, FromRepr)]
#[repr(u32)]
pub enum OutputTransform {
    /// No transform applied
    Normal = 0,
    /// Rotated 90 degrees
    Rotate90 = 1,
    /// Rotated 180 degrees
    Rotate180 = 2,
    /// Rotated 270 degrees
    Rotate270 = 3,
    /// Flipped
    Flipped = 4,
    /// Flipped and rotated 90 degrees
    Flipped90 = 5,
    /// Flipped and rotated 180 degrees
    Flipped180 = 6,
    /// Flipped and rotated 270 degrees
    Flipped270 = 7,
}

/// The arrangement of subpixels on the display
#[derive(Debug, Clone, Copy, FromRepr)]
#[repr(u32)]
pub enum OutputSubpixel {
    /// Unknown
    Unknown = 0,
    /// Explicitly not applicable (e.g. for winit)
    None = 1,
    /// Subpixels are horizontal in RGB order
    HorizontalRgb = 2,
    /// Subpixels are horizontal in BGR order
    HorizontalBgr = 3,
    /// Subpixels are vertical in RGB order
    VerticalRgb = 4,
    /// Subpixels are vertical in BGR order
    VerticalBgr = 5,
}

/// Output geometry
#[derive(Debug, Clone)]
pub struct OutputGeometry {
    /// The top-left pixel of this output's x location in global space
    pub x: i32,
    /// The top-left pixel of this output's y location in global space
    pub y: i32,
    /// The physical width in pixels of this output
    pub physical_width: i32,
    /// The physical height in pixels of this output
    pub physical_height: i32,
    /// The subpixel spec for this output
    pub subpixel: OutputSubpixel,
    /// The make of this output/monitor
    pub make: String,
    /// The model of this output/monitor
    pub model: String,
    /// The transform applied to this output
    pub transform: OutputTransform,
}

/// The mode of an output/monitor
#[derive(Debug, Clone)]
pub struct OutputMode {
    /// Flags indicating additional details of this mode:
    ///   - `OUTPUT_MODE_CURRENT`
    ///   - `OUTPUT_MODE_PREFERRED`
    pub flags: u32,
    /// Width for this mode
    pub width: i32,
    /// Height for this mode
    pub height: i32,
    /// Refresh rate in mhz
    pub refresh_mhz: i32,
}

/// A unique output id
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(pub u32);

/// An output/monitor available to this backend
#[derive(Debug, Clone)]
pub struct Output {
    /// The output id
    pub id: OutputId,
    /// The geometry for this output
    pub geometry: OutputGeometry,
    /// The modes for this output
    pub modes: Vec<OutputMode>,
    /// The scale of this output
    pub scale: i32,
    /// The name for this output
    pub name: String,
    /// The description for this output
    pub description: String,
}

/// The positions a cursor may occupy on an output, as an inclusive rectangle.
pub fn cursor_bounds(output: &Output) -> Option<(f64, f64, f64, f64)> {
    let g = &output.geometry;
    if g.physical_width <= 0 || g.physical_height <= 0 {
        return None;
    }
    Some((
        f64::from(g.x),
        f64::from(g.y),
        f64::from(g.x + g.physical_width - 1),
        f64::from(g.y + g.physical_height - 1),
    ))
}

/// Whether an output's area contains a point, in global coordinates.
pub fn output_contains(output: &Output, x: i32, y: i32) -> bool {
    let g = &output.geometry;
    x >= g.x && x < g.x + g.physical_width && y >= g.y && y < g.y + g.physical_height
}
