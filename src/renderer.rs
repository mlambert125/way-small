//! Software renderer.
//!
//! Composites all surfaces into a single framebuffer using CPU-based
//! rendering. Reads pixel data from client shm pools via mmap, handles
//! subsurface tree traversal, and performs pre-multiplied alpha blending.

use tracing::{debug, info};

use crate::backend::{BACKGROUND_COLOR, RenderFrame};
use crate::protocol::state::{ClientObjectId, DefaultCursor, OUTPUT_MODE_CURRENT, Output};
use crate::protocol::{self, CompositorState};

const BYTES_PER_PIXEL: u64 = 4;
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

const CURSOR_BLACK: u32 = 0xff00_0000;
const CURSOR_WHITE: u32 = 0xffff_ffff;

/// Blend a premultiplied-alpha source pixel onto a destination pixel.
#[inline]
fn blend_premultiplied(dst: u32, src: u32, alpha: u32) -> u32 {
    let inv_alpha = 255 - alpha;
    let dst_rb = ((dst & 0x00ff_00ff) * inv_alpha) >> 8;
    let rb = (src & 0x00ff_00ff) + (dst_rb & 0x00ff_00ff);
    let dst_g = ((dst & 0x0000_ff00) * inv_alpha) >> 8;
    let g = (src & 0x0000_ff00) + (dst_g & 0x0000_ff00);
    0xff00_0000 | (rb & 0x00ff_00ff) | (g & 0x0000_ff00)
}

pub fn render(output: &Output, state: &CompositorState) -> RenderFrame {
    let mode = output
        .modes
        .iter()
        .find(|m| m.flags & OUTPUT_MODE_CURRENT != 0)
        .expect("Output has no current mode");
    let width = mode.width;
    let height = mode.height;

    let mut pixels = vec![BACKGROUND_COLOR; (width * height) as usize];

    for &key in &state.surface_stack {
        let (ox, oy) = state.surfaces.get(&key).map_or((0, 0), |s| s.position);
        blit_surface_tree(state, &mut pixels, width, height, key, ox, oy);
    }

    let cx = state.cursor_x as i32;
    let cy = state.cursor_y as i32;
    if !blit_client_cursor(state, &mut pixels, width, height, cx, cy) {
        if let Some(ref cursor) = state.default_cursor {
            blit_default_cursor(cursor, &mut pixels, width, height, cx, cy);
        } else {
            blit_fallback_mouse_cursor(&mut pixels, width, height, cx, cy);
        }
    }

    RenderFrame {
        output_id: output.id,
        pixels,
        width,
        height,
    }
}

fn blit_surface_tree(
    state: &protocol::CompositorState,
    pixels: &mut [u32],
    width: i32,
    height: i32,
    surface_key: ClientObjectId,
    offset_x: i32,
    offset_y: i32,
) {
    let Some(surface) = state.surfaces.get(&surface_key) else {
        return;
    };

    let client_id = surface.client_id;
    let children = surface.children.clone();

    blit_surface_buffer(
        state,
        pixels,
        width,
        height,
        surface_key,
        offset_x,
        offset_y,
    );

    for child_id in children {
        let child_key = (client_id, child_id);
        let Some(child) = state.surfaces.get(&child_key) else {
            continue;
        };
        let (cx, cy) = child.subsurface_position;
        blit_surface_tree(
            state,
            pixels,
            width,
            height,
            child_key,
            offset_x + cx,
            offset_y + cy,
        );
    }
}

#[allow(clippy::too_many_lines)]
fn blit_surface_buffer(
    state: &protocol::CompositorState,
    pixels: &mut [u32],
    width: i32,
    height: i32,
    surface_key: ClientObjectId,
    offset_x: i32,
    offset_y: i32,
) {
    let Some(surface) = state.surfaces.get(&surface_key) else {
        return;
    };
    let client_id = surface.client_id;
    let Some(buffer_id) = surface.buffer_id else {
        return;
    };
    let Some(shm_buffer) = state.shm_buffers.get(&(client_id, buffer_id)) else {
        return;
    };
    let Some(pool) = state.shm_pools.get(&(client_id, shm_buffer.pool_id)) else {
        return;
    };

    let viewport = state
        .surface_viewport
        .get(&(client_id, surface_key.1))
        .and_then(|&vp_id| state.viewports.get(&(client_id, vp_id)));

    if shm_buffer.width <= 0 || shm_buffer.height <= 0 {
        return;
    }

    let ptr = pool.map_ptr;
    if ptr.is_null() {
        debug!("Pool has no valid mapping for surface {:?}", surface_key);
        return;
    }

    // The size of the last row of the buffer (this can be smaller than stride*bytes_per_pixel as
    // the last row doesn't require padding)
    let last_row_size = shm_buffer.width as u64 * BYTES_PER_PIXEL;

    // The end offset of the buffer data in the pool. This must be <= pool.size
    // or it would be reading past the end of the mapping
    let buf_end = shm_buffer.offset as u64
        + (shm_buffer.height as u64 - 1) * shm_buffer.stride as u64
        + last_row_size;
    if buf_end > pool.size as u64 {
        debug!(
            "Buffer exceeds pool size: end={} pool_size={} surface={:?}",
            buf_end, pool.size, surface_key
        );
        return;
    }

    let buf_w = shm_buffer.width as usize;
    let buf_h = shm_buffer.height as usize;
    let stride = shm_buffer.stride as usize;
    let buf_offset = shm_buffer.offset as usize;
    let dst_w = width as usize;
    let dst_h = height as usize;

    // Calculate the source rectangle in the buffer and the destination size on the surface.
    // This is based on the viewport if present, or defaults to the entire buffer at 1:1 scale.

    // Source rectangle (crop) in buffer pixel coordinates
    let (src_x0, src_y0, src_w, src_h) = match viewport.and_then(|v| v.source) {
        Some((sx, sy, sw, sh)) => (sx, sy, sw, sh),
        None => (0.0, 0.0, buf_w as f64, buf_h as f64),
    };

    // Destination size in surface coordinates
    let (dest_w, dest_h) = match viewport.and_then(|v| v.destination) {
        Some((dw, dh)) => (dw as usize, dh as usize),
        None => (src_w as usize, src_h as usize),
    };

    for dy in 0..dest_h {
        let dst_y = offset_y as isize + dy as isize;
        if dst_y < 0 || dst_y >= dst_h as isize {
            continue;
        }
        let dst_row_start = dst_y as usize * dst_w;

        // Map destination y back to source buffer y
        let sy = src_y0 + (dy as f64 + 0.5) * src_h / dest_h as f64;
        let src_yi = sy as usize;
        if src_yi >= buf_h {
            continue;
        }

        let src_row = unsafe {
            std::slice::from_raw_parts(
                (ptr as *const u8).add(buf_offset + src_yi * stride),
                buf_w * 4,
            )
        };

        for dx in 0..dest_w {
            let dst_x = offset_x as isize + dx as isize;
            if dst_x < 0 || dst_x >= dst_w as isize {
                continue;
            }

            // Map destination x back to source buffer x
            let sx = src_x0 + (dx as f64 + 0.5) * src_w / dest_w as f64;
            let src_xi = sx as usize;
            if src_xi >= buf_w {
                continue;
            }

            let src = u32::from_le_bytes([
                src_row[src_xi * 4],
                src_row[src_xi * 4 + 1],
                src_row[src_xi * 4 + 2],
                src_row[src_xi * 4 + 3],
            ]);

            // XRGB8888 (format 1): high byte is undefined, treat as fully opaque
            let alpha = if shm_buffer.format == 1 {
                255
            } else {
                (src >> 24) & 0xff
            };

            // Destination index (calculated from loops)
            let dst_idx = dst_row_start + dst_x as usize;

            if alpha == 255 {
                pixels[dst_idx] = src | 0xff00_0000;
            } else if alpha != 0 {
                pixels[dst_idx] = blend_premultiplied(pixels[dst_idx], src, alpha);
            }
        }
    }
}

/// Try to render the focused client's cursor surface. Returns true if a client cursor
/// was used (including hidden cursors), false to fall back to the hardcoded bitmap.
fn blit_client_cursor(
    state: &protocol::CompositorState,
    pixels: &mut [u32],
    width: i32,
    height: i32,
    cx: i32,
    cy: i32,
) -> bool {
    // Determine which client currently has pointer focus.
    let Some((pointer_client, _)) = state.pointer_surface else {
        return false;
    };

    // Look up that client's cursor surface.
    match state.cursor_surfaces.get(&pointer_client) {
        Some(None) => true, // Client wants hidden cursor
        Some(&Some((surface_id, hotspot_x, hotspot_y))) => {
            let surface_key = (pointer_client, surface_id);
            if state
                .surfaces
                .get(&surface_key)
                .and_then(|s| s.buffer_id)
                .is_none()
            {
                return false; // No buffer attached yet
            }
            blit_surface_buffer(
                state,
                pixels,
                width,
                height,
                surface_key,
                cx - hotspot_x,
                cy - hotspot_y,
            );
            true
        }
        None => false, // Client hasn't set a cursor
    }
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
        .min_by_key(|img| (img.size as i32 - target_size as i32).unsigned_abs())?;

    // The xcursor file stores pixels as little-endian 32-bit ARGB (premultiplied alpha).
    // The crate's `pixels_rgba` is the raw file bytes: [B, G, R, A] per pixel on LE systems.
    // Reading as LE u32 gives us 0xAARRGGBB directly, matching our framebuffer format.
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
        width: image.width as i32,
        height: image.height as i32,
        hotspot_x: image.xhot as i32,
        hotspot_y: image.yhot as i32,
    })
}

/// Blit a pre-loaded theme cursor onto the framebuffer with premultiplied alpha blending.
fn blit_default_cursor(
    cursor: &DefaultCursor,
    pixels: &mut [u32],
    width: i32,
    height: i32,
    cx: i32,
    cy: i32,
) {
    let draw_x = cx - cursor.hotspot_x;
    let draw_y = cy - cursor.hotspot_y;

    for sy in 0..cursor.height {
        let dy = draw_y + sy;
        if dy < 0 || dy >= height {
            continue;
        }
        for sx in 0..cursor.width {
            let dx = draw_x + sx;
            if dx < 0 || dx >= width {
                continue;
            }

            let src = cursor.pixels[(sy * cursor.width + sx) as usize];
            let alpha = (src >> 24) & 0xff;

            if alpha == 0 {
                continue;
            }

            let dst_idx = (dy * width + dx) as usize;
            if alpha == 255 {
                pixels[dst_idx] = src;
            } else {
                pixels[dst_idx] = blend_premultiplied(pixels[dst_idx], src, alpha);
            }
        }
    }
}

fn blit_fallback_mouse_cursor(pixels: &mut [u32], width: i32, height: i32, cx: i32, cy: i32) {
    for (row_idx, row) in FALLBACK_CURSOR_BITMAP.iter().enumerate() {
        let dy = cy + row_idx as i32;
        if dy < 0 || dy >= height {
            continue;
        }
        for (col_idx, &ch) in row.iter().enumerate() {
            let dx = cx + col_idx as i32;
            if dx < 0 || dx >= width {
                continue;
            }
            let color = match ch {
                b'B' => CURSOR_BLACK,
                b'W' => CURSOR_WHITE,
                _ => continue,
            };
            pixels[dy as usize * width as usize + dx as usize] = color;
        }
    }
}
