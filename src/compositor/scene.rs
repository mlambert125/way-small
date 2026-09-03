//! Scene building.
//!
//! Turns compositor state into a `Scene`: a flat, back-to-front list of
//! textured quads in output pixel coordinates. Surface position, subsurface
//! offsets, `wp_viewport` cropping and scaling, and buffer scale all resolve
//! here into a source rectangle paired with a destination rectangle. Runs in
//! the compositor task, which owns the state; the backend turns the result
//! into GPU work in its own thread and GL context.
//!
//! No client pixels are copied. A texture points straight into the client's
//! shm mapping and holds that buffer's guard, which is what stops
//! `wl_buffer.release` going out while the backend is still reading. The
//! `SceneCache` therefore holds no pixels either — only enough about the last
//! frame to say what the backend already has, so damage can be expressed
//! against it.

use super::protocol::CompositorState;
use super::protocol::state::{Buffer, BufferKind, ClientObjectId, DefaultCursor};
use super::protocol::wire_utils::f64_to_i32;
use super::protocol::wl_shm::FORMAT_XRGB8888;
use crate::shared::{OUTPUT_MODE_CURRENT, Output, PoolMapping, output_contains, pixel_format};
use crate::shared::{
    PixelFormat, Scene, SceneElement, TextureId, TextureImage, TextureSource, UploadPixels,
};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info};

#[cfg(test)]
mod tests;

/// Number of bytes for representing one pixel
const BYTES_PER_PIXEL: usize = 4;

/// Fallback mouse cursor image encoded as an ASCII string
const FALLBACK_CURSOR_BITMAP: [&[u8; 12]; 19] = [
    b"B...........",
    b"BB..........",
    b"BWB.........",
    b"BWWB........",
    b"BWWWB.......",
    b"BWWWWB......",
    b"BWWWWWB.....",
    b"BWWWWWWB....",
    b"BWWWWWWWB...",
    b"BWWWWWWWWB..",
    b"BWWWWWWWWWB.",
    b"BWWWWWWWWWWB",
    b"BWWWWWWBBBBB",
    b"BWWBWWWB....",
    b"BWBB.BWWB...",
    b"BB...BWWB...",
    b"B.....BWWB..",
    b"......BWWB..",
    b".......BB...",
];

/// Black color for coloring the fallback cursor
const CURSOR_BLACK: u32 = 0xff00_0000;
/// White color for coloring the fallback cursor
const CURSOR_WHITE: u32 = 0xffff_ffff;
/// The width of the fallback cursor
const FALLBACK_CURSOR_WIDTH: i32 = 12;

/// A choice of how to render the cursor (hidden, client-provided surface, or compositor
/// default/theme.)
enum CursorChoice {
    /// The focused client asked for no cursor.
    Hidden,
    /// Draw the client's own cursor surface at the given hotspot.
    Surface(ClientObjectId, i32, i32),
    /// The compositor picks: theme cursor, or the built-in one.
    Compositor,
}

/// Pixel copies of client buffers and cursors, kept across frames.
///
/// Lives with the compositor loop rather than in `CompositorState`: it is
/// derived from protocol state, never part of it, and nothing in the protocol
/// layer needs to know it exists. Protocol handlers signal a content change by
/// bumping `ShmBuffer::content_serial`; this compares serials and re-reads only
/// what actually moved.
/// What the cache remembers about a buffer between frames.
///
/// Metadata only. There are no pixels to keep now that images borrow the
/// client's mapping, and holding a `TextureImage` here would pin the client's
/// buffer forever and stall its release.
#[derive(Clone, Copy)]
struct CachedImage {
    /// A unique serial number for this cached image
    serial: u64,
    /// Width of the cached image
    width: i32,
    /// Height of the cached image
    height: i32,
}

/// Cached textures
#[derive(Default)]
pub struct SceneCache {
    /// Buffers from the client
    buffers: HashMap<ClientObjectId, CachedImage>,
    /// Cursor textures
    cursors: HashMap<TextureId, Arc<TextureImage>>,
    /// Serials for cursor images, which have no protocol-side content serial.
    next_cursor_serial: u64,
}

impl SceneCache {
    /// Create a new instance of the scene cache with defaults
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the cache is holding no buffer copies.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }

    /// Drop copies of buffers that no longer exist.
    ///
    /// Buffers are destroyed without the cache hearing about it, so entries are
    /// reaped against live state rather than by explicit removal.
    pub fn gc(&mut self, state: &CompositorState) {
        self.buffers
            .retain(|key, _| state.buffers.contains_key(key));
    }
}

/// Build the scene for one output.
pub fn build(
    output_id: crate::shared::OutputId,
    serial: u64,
    state: &CompositorState,
    cache: &mut SceneCache,
) -> Scene {
    let mut elements = Vec::new();

    // An output with no current mode has nothing to compose onto.
    if let Some(output) = state.outputs.iter().find(|o| o.id == output_id)
        && current_mode_size(output).is_some()
    {
        // Windows are positioned globally but drawn in the coordinates of the
        // output they are on, so everything shifts by that output's origin.
        let (origin_x, origin_y) = (output.geometry.x, output.geometry.y);

        // Only the workspace showing on this output is drawn, and it holds
        // only that output's windows. Popups and subsurfaces come along with
        // their toplevel.
        for &key in state.workspaces.visible_stack(output_id) {
            let Some(surface) = state.surfaces.get(&key) else {
                continue;
            };
            let (x, y) = surface.position;
            push_surface_tree(state, cache, &mut elements, key, x - origin_x, y - origin_y);
        }

        // Above every window and below the cursor: a drag icon is meant to be
        // the thing being carried, and the pointer stays on top of it.
        push_drag_icon(state, cache, &mut elements, output, origin_x, origin_y);
        push_cursor(state, cache, &mut elements, output, origin_x, origin_y);
    }

    Scene {
        output_id,
        serial,
        elements,
    }
}

/// Get the size (resolution) of the current mode of the provided output
fn current_mode_size(output: &Output) -> Option<(i32, i32)> {
    output
        .modes
        .iter()
        .find(|m| m.flags & OUTPUT_MODE_CURRENT != 0)
        .map(|m| (m.width, m.height))
}

/// Push the entire surface tree into `elements` to build
/// the scene graph to be passed to the backend
fn push_surface_tree(
    state: &CompositorState,
    cache: &mut SceneCache,
    elements: &mut Vec<SceneElement>,
    surface_key: ClientObjectId,
    offset_x: i32,
    offset_y: i32,
) {
    let Some(surface) = state.surfaces.get(&surface_key) else {
        return;
    };
    let client_id = surface.client_id;

    push_surface(state, cache, elements, surface_key, offset_x, offset_y);

    for &child_id in &surface.children {
        let child_key = (client_id, child_id);
        let Some(child) = state.surfaces.get(&child_key) else {
            continue;
        };
        let (cx, cy) = child.subsurface_position;
        push_surface_tree(
            state,
            cache,
            elements,
            child_key,
            offset_x.saturating_add(cx),
            offset_y.saturating_add(cy),
        );
    }
}

/// Add one surface's buffer to the scene, if it has one.
///
/// The source crop and destination size come from the same mapping the damage
/// path runs backwards, expressed here as a single quad instead of a per-pixel
/// sampling loop. Clipping to the output is left to the GPU.
fn push_surface(
    state: &CompositorState,
    cache: &mut SceneCache,
    elements: &mut Vec<SceneElement>,
    surface_key: ClientObjectId,
    offset_x: i32,
    offset_y: i32,
) {
    let Some(surface) = state.surfaces.get(&surface_key) else {
        return;
    };
    let Some(buffer_id) = surface.buffer_id else {
        return;
    };
    let Some(mapping) = state.surface_buffer_mapping(surface_key) else {
        return;
    };
    let Some(texture) = ensure_image(state, cache, (surface.client_id, buffer_id)) else {
        return;
    };

    elements.push(SceneElement {
        texture,
        src: mapping.src,
        dst: (offset_x, offset_y, mapping.dest_width, mapping.dest_height),
    });
}

/// Build the texture for a client buffer, however its memory is held.
///
/// The one place the two kinds of buffer part company: an upload has to be
/// read, bounds-checked and diffed against what the backend already has, while
/// an imported one is handed over as a description and sampled where it lies.
fn ensure_image(
    state: &CompositorState,
    cache: &mut SceneCache,
    key: ClientObjectId,
) -> Option<Arc<TextureImage>> {
    match &state.buffers.get(&key)?.kind {
        BufferKind::Shm(_) => ensure_buffer_image(state, cache, key),
        BufferKind::Dmabuf(image) => Some(imported_image(key, state.buffers.get(&key)?, image)),
        // Described, accepted, and then refused by the driver. It draws
        // nothing rather than taking the client down for its driver's answer.
        BufferKind::Failed => None,
    }
}

/// Describe an already-imported buffer to the backend.
///
/// Nothing is read and nothing is cached: the texture the backend builds from
/// this samples the client's own memory, so a client drawing into it changes
/// what is on screen without anything passing through here. That is also why
/// the serial never moves — see [`crate::compositor::protocol::state::Buffer`].
fn imported_image(
    key: ClientObjectId,
    buffer: &Buffer,
    image: &Arc<crate::shared::DmabufImage>,
) -> Arc<TextureImage> {
    Arc::new(TextureImage {
        id: TextureId::Buffer(key.0, key.1),
        serial: buffer.content_serial,
        width: buffer.width,
        height: buffer.height,
        format: pixel_format(image.fourcc),
        source: TextureSource::Dmabuf(image.clone()),
    })
}

/// Build the texture for a client buffer, borrowing the client's mapping.
///
/// No pixels are copied: the image points into the shm mapping and holds the
/// buffer's guard, which is what keeps `wl_buffer.release` from being sent
/// while the backend is still reading. The cache is consulted only to work out
/// what the backend already has, so damage can be expressed against it.
fn ensure_buffer_image(
    state: &CompositorState,
    cache: &mut SceneCache,
    key: ClientObjectId,
) -> Option<Arc<TextureImage>> {
    let buffer = state.buffers.get(&key)?;
    // Only an upload comes through here. A dma-buf is imported instead, and
    // none of the mapping, stride or damage reasoning below applies to it.
    let shm = buffer.shm()?;
    if buffer.width <= 0 || buffer.height <= 0 {
        return None;
    }
    let pool = state.shm_pools.get(&(key.0, shm.pool_id))?;
    let Some(mapping) = pool.mapping.as_ref() else {
        debug!("Pool has no valid mapping for buffer {key:?}");
        return None;
    };

    let width = buffer.width.unsigned_abs() as usize;
    let height = buffer.height.unsigned_abs() as usize;
    let stride = shm.stride.unsigned_abs() as usize;
    let offset = shm.offset.unsigned_abs() as usize;
    let row_bytes = width * BYTES_PER_PIXEL;
    if stride < row_bytes {
        debug!("Buffer stride {stride} is shorter than its rows for {key:?}");
        return None;
    }

    // The end offset of the buffer data in the pool. The last row needs no
    // stride padding, so this is less than `offset + height * stride`. It must
    // be within the mapping or every read past it is out of bounds.
    let extent = offset + (height - 1) * stride + row_bytes;
    if extent > mapping.size() {
        debug!(
            "Buffer exceeds pool mapping: end={extent} mapping={} buffer={key:?}",
            mapping.size()
        );
        return None;
    }

    let previous = cache.buffers.get(&key).copied();

    // Damage describes a change *from* the image the backend already holds, so
    // it is only usable if there is one and it is the same shape. A resized
    // buffer shares nothing with its predecessor even where rectangles overlap.
    let unchanged = previous.is_some_and(|p| p.serial == buffer.content_serial);
    let damage = match previous {
        Some(p) if !unchanged && p.width == buffer.width && p.height == buffer.height => {
            shm.damage.clone().unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let previous_serial = (!unchanged).then_some(previous.map(|p| p.serial)).flatten();

    // GL addresses rows in whole pixels, so a stride that is not a multiple of
    // four cannot be described to it. Repacking is the only way to draw such a
    // buffer at all; no real toolkit produces one.
    let pixels = if stride.is_multiple_of(BYTES_PER_PIXEL) {
        UploadPixels::Mapped {
            guard: state.buffer_guards.get(&key)?.clone(),
            offset,
            stride,
        }
    } else {
        UploadPixels::Owned(repack_rows(mapping, offset, stride, row_bytes, height)?)
    };

    let image = Arc::new(TextureImage {
        id: TextureId::Buffer(key.0, key.1),
        serial: buffer.content_serial,
        width: buffer.width,
        height: buffer.height,
        format: if shm.format == FORMAT_XRGB8888 {
            PixelFormat::Xrgb8888
        } else {
            PixelFormat::Argb8888
        },
        source: TextureSource::Upload {
            pixels,
            previous_serial,
            damage,
        },
    });
    cache.buffers.insert(
        key,
        CachedImage {
            serial: buffer.content_serial,
            width: buffer.width,
            height: buffer.height,
        },
    );
    Some(image)
}

/// Copy a buffer's rows out of the mapping, tightly packed.
///
/// Only for layouts GL cannot read in place; the normal path takes no copy.
fn repack_rows(
    mapping: &PoolMapping,
    offset: usize,
    stride: usize,
    row_bytes: usize,
    height: usize,
) -> Option<Box<[u8]>> {
    let mut out = vec![0u8; height * row_bytes];
    for y in 0..height {
        // SAFETY: the caller checked the buffer's extent against the mapping,
        // and the client may not write to a committed buffer before release.
        let src = unsafe { mapping.slice(offset + y * stride, row_bytes)? };
        out[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(src);
    }
    Some(out.into_boxed_slice())
}

/// Add the icon of a drag in progress to the scene.
///
/// The icon follows the pointer, offset by whatever the client attached it
/// with. That offset is the only means a client has to position its icon — a
/// toolkit centres one under the cursor by attaching at a negative dx and dy —
/// which is why [`crate::compositor::protocol::state::Surface::offset`] is
/// tracked at all.
///
/// Nothing has to be unwound when the drag ends: this reads `state.drag`, so
/// clearing the drag stops drawing the icon on the next frame.
fn push_drag_icon(
    state: &CompositorState,
    cache: &mut SceneCache,
    elements: &mut Vec<SceneElement>,
    output: &Output,
    origin_x: i32,
    origin_y: i32,
) {
    let Some(icon) = state.drag.as_ref().and_then(|drag| drag.icon) else {
        return;
    };
    let cx = f64_to_i32(state.cursor_x);
    let cy = f64_to_i32(state.cursor_y);
    // The pointer is over one output at a time, and so is what it is carrying.
    if !output_contains(output, cx, cy) {
        return;
    }
    let offset = state.surfaces.get(&icon).map_or((0, 0), |s| s.offset);

    // The whole tree: an icon surface can never itself be a subsurface, but it
    // may have them.
    push_surface_tree(
        state,
        cache,
        elements,
        icon,
        cx - origin_x + offset.0,
        cy - origin_y + offset.1,
    );
}

/// Add the pointer cursor to the scene.
///
/// A client that has set its own cursor surface gets that; a client that asked
/// for a hidden cursor gets nothing. Otherwise the compositor draws the theme
/// cursor, or its built-in one if no theme loaded.
fn push_cursor(
    state: &CompositorState,
    cache: &mut SceneCache,
    elements: &mut Vec<SceneElement>,
    output: &Output,
    origin_x: i32,
    origin_y: i32,
) {
    let cx = f64_to_i32(state.cursor_x);
    let cy = f64_to_i32(state.cursor_y);
    // The pointer is over one output at a time; the others draw no cursor.
    if !output_contains(output, cx, cy) {
        return;
    }
    let (cx, cy) = (cx - origin_x, cy - origin_y);

    match client_cursor(state) {
        CursorChoice::Hidden => return,
        CursorChoice::Surface(surface_key, hotspot_x, hotspot_y) => {
            push_surface(
                state,
                cache,
                elements,
                surface_key,
                cx - hotspot_x,
                cy - hotspot_y,
            );
            return;
        }
        CursorChoice::Compositor => {}
    }

    let (id, hotspot_x, hotspot_y) = match state.default_cursor {
        Some(ref cursor) => (TextureId::DefaultCursor, cursor.hotspot_x, cursor.hotspot_y),
        None => (TextureId::FallbackCursor, 0, 0),
    };
    let Some(texture) = ensure_cursor_image(state, cache, id) else {
        return;
    };
    let (w, h) = (texture.width, texture.height);
    elements.push(SceneElement {
        texture,
        src: (0.0, 0.0, f64::from(w), f64::from(h)),
        dst: (cx - hotspot_x, cy - hotspot_y, w, h),
    });
}

/// Look at compositor state and decide on a mouse cursor to display
fn client_cursor(state: &CompositorState) -> CursorChoice {
    let Some((pointer_client, _)) = state.pointer_surface else {
        return CursorChoice::Compositor;
    };
    match state.cursor_surfaces.get(&pointer_client) {
        Some(None) => CursorChoice::Hidden,
        Some(&Some((surface_id, hotspot_x, hotspot_y))) => {
            let surface_key = (pointer_client, surface_id);
            // No buffer attached yet — fall back rather than show nothing.
            if state
                .surfaces
                .get(&surface_key)
                .and_then(|s| s.buffer_id)
                .is_none()
            {
                return CursorChoice::Compositor;
            }
            CursorChoice::Surface(surface_key, hotspot_x, hotspot_y)
        }
        None => CursorChoice::Compositor,
    }
}

/// Return the compositor's own cursor image, building it on first use.
fn ensure_cursor_image(
    state: &CompositorState,
    cache: &mut SceneCache,
    id: TextureId,
) -> Option<Arc<TextureImage>> {
    if let Some(image) = cache.cursors.get(&id) {
        return Some(image.clone());
    }

    let (width, height, argb) = match id {
        TextureId::DefaultCursor => {
            let cursor = state.default_cursor.as_ref()?;
            (cursor.width, cursor.height, cursor.pixels.clone())
        }
        TextureId::FallbackCursor => fallback_cursor_pixels(),
        TextureId::Buffer(..) => return None,
    };

    cache.next_cursor_serial += 1;
    let image = Arc::new(TextureImage {
        id,
        serial: cache.next_cursor_serial,
        width,
        height,
        format: PixelFormat::Argb8888,
        source: TextureSource::Upload {
            pixels: UploadPixels::Owned(argb_to_bytes(&argb)),
            // Cursor images never change once built, so there is nothing to
            // patch and nothing to patch against.
            previous_serial: None,
            damage: Vec::new(),
        },
    });
    cache.cursors.insert(id, image.clone());
    Some(image)
}

/// Rasterise the built-in cursor bitmap into premultiplied ARGB pixels.
fn fallback_cursor_pixels() -> (i32, i32, Vec<u32>) {
    let height = i32::try_from(FALLBACK_CURSOR_BITMAP.len()).unwrap_or(0);
    let mut pixels =
        Vec::with_capacity(FALLBACK_CURSOR_BITMAP.len() * FALLBACK_CURSOR_WIDTH as usize);
    for row in FALLBACK_CURSOR_BITMAP {
        for &ch in row {
            pixels.push(match ch {
                b'B' => CURSOR_BLACK,
                b'W' => CURSOR_WHITE,
                // Fully transparent, and premultiplied, so it blends to nothing.
                _ => 0,
            });
        }
    }
    (FALLBACK_CURSOR_WIDTH, height, pixels)
}

/// Flatten `0xAARRGGBB` words into the little-endian `[B, G, R, A]` byte order
/// that shm buffers already use, so both take the same upload path.
fn argb_to_bytes(pixels: &[u32]) -> Box<[u8]> {
    let mut out = Vec::with_capacity(pixels.len() * BYTES_PER_PIXEL);
    for &p in pixels {
        out.extend_from_slice(&p.to_le_bytes());
    }
    out.into_boxed_slice()
}

/// Load the default cursor from the system cursor theme.
///
/// Reads `$XCURSOR_THEME` (default: "default") and `$XCURSOR_SIZE` (default: 24),
/// loads the `left_ptr` cursor, and converts the pixel data to ARGB u32 format.
pub fn load_default_cursor() -> Option<DefaultCursor> {
    let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_string());
    let target_size = std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(24);

    let theme = xcursor::CursorTheme::load(&theme_name);
    let cursor_path = theme.load_icon("left_ptr")?;
    let content = std::fs::read(&cursor_path).ok()?;
    let images = xcursor::parser::parse_xcursor(&content)?;

    // Pick the image closest to the requested size.
    let image = images
        .iter()
        .min_by_key(|img| (img.size.cast_signed() - target_size.cast_signed()).unsigned_abs())?;

    // The xcursor file stores pixels as little-endian 32-bit ARGB (premultiplied alpha).
    // The crate's `pixels_rgba` is the raw file bytes: [B, G, R, A] per pixel on LE systems.
    // Reading as LE u32 gives us 0xAARRGGBB directly, matching our texture format.
    let pixels: Vec<u32> = image
        .pixels_rgba
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    info!(
        "Loaded cursor theme '{}': {}x{} hotspot=({},{}) from {:?}",
        theme_name, image.width, image.height, image.xhot, image.yhot, cursor_path
    );

    Some(DefaultCursor {
        pixels,
        width: image.width.cast_signed(),
        height: image.height.cast_signed(),
        hotspot_x: image.xhot.cast_signed(),
        hotspot_y: image.yhot.cast_signed(),
    })
}
