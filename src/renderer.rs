//! Software renderer.
//!
//! Composites all surfaces into a single framebuffer using CPU-based
//! rendering. Reads pixel data from client shm pools via mmap, handles
//! subsurface tree traversal, and performs pre-multiplied alpha blending.

use tracing::debug;

use crate::backend::RenderFrame;
use crate::protocol;
use crate::protocol::state::ClientObjectId;

const BACKGROUND_COLOR: u32 = 0xff1a_1a2e;
const BYTES_PER_PIXEL: u64 = 4;

/// Composite all surfaces into a single framebuffer.
pub fn render(state: &protocol::CompositorState, width: u32, height: u32) -> RenderFrame {
    let mut pixels = vec![BACKGROUND_COLOR; (width * height) as usize];

    // Draw surfaces in stack order (bottom to top)
    for &key in &state.surface_stack {
        let (ox, oy) = state.surfaces.get(&key).map(|s| s.position).unwrap_or((0, 0));
        blit_surface_tree(state, &mut pixels, width, height, key, ox, oy);
    }

    // Draw the hardware cursor last so that it's on top
    blit_cursor(
        &mut pixels,
        width,
        height,
        state.cursor_x as i32,
        state.cursor_y as i32,
    );

    RenderFrame {
        pixels,
        width,
        height,
    }
}

/// Recursively blit a surface and its subsurfaces at the given offset.
fn blit_surface_tree(
    state: &protocol::CompositorState,
    pixels: &mut [u32],
    width: u32,
    height: u32,
    surface_key: ClientObjectId,
    offset_x: i32,
    offset_y: i32,
) {
    let Some(surface) = state.surfaces.get(&surface_key) else {
        return;
    };

    let client_id = surface.client_id;
    let children = surface.children.clone();

    // Blit this surface's buffer
    blit_surface_buffer(state, pixels, width, height, surface_key, offset_x, offset_y);

    // Blit children at their positions
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

/// Blit a single surface's buffer into the framebuffer at the given offset.
fn blit_surface_buffer(
    state: &protocol::CompositorState,
    pixels: &mut [u32],
    width: u32,
    height: u32,
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

    // Look up viewport for this surface (crop + scale)
    // This is optional, and will only be present if the client set a viewport
    // on this surface.  If there isn't one, we just use the entire buffer at 1:1 scale.
    let viewport = state
        .viewports
        .values()
        .find(|v| v.client_id == client_id && v.surface_id == surface_key.1);

    // Validate the fd is still valid and large enough
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(pool.fd, &mut stat) } != 0 {
        debug!("Pool fd {} is invalid for surface {:?}", pool.fd, surface_key);
        return;
    }
    // The actual size of the entire pool file
    let actual_size = stat.st_size as u64;

    // The size of the last row of the buffer (this can be smaller than stride*bytes_per_pixel as
    // the last row doesn't require padding)
    let last_row_size = shm_buffer.width as u64 * BYTES_PER_PIXEL;

    // The end offset of the buffer data in the pool file. This must be <= actual_size
    // or it would be reading past the end of the fd
    let buf_end = shm_buffer.offset as u64
        + (shm_buffer.height as u64 - 1) * shm_buffer.stride as u64
        + last_row_size;
    if buf_end > actual_size {
        debug!(
            "Buffer exceeds actual file size: end={} file_size={} pool_size={} surface={:?}",
            buf_end, actual_size, pool.size, surface_key
        );
        return;
    }

    // mmap the pool file to read pixel data. We only need PROT_READ since we're not modifying it.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            pool.size as usize,
            libc::PROT_READ,
            libc::MAP_SHARED,
            pool.fd,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        debug!("Failed to mmap pool for surface {:?}", surface_key);
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

            // If the source pixel is fully opaque, just copy it.
            if alpha == 255 {
                pixels[dst_idx] = src | 0xff000000;
            // If the source pixel is fully transparent, skip it.
            } else if alpha == 0 {
                continue;
            // Otherwise, blend the source pixel with the destination pixel using pre-multiplied alpha.
            } else {
                let dst = pixels[dst_idx];
                let inv_alpha = 255 - alpha;

                let dst_rb = ((dst & 0x00ff00ff) * inv_alpha) >> 8;
                let rb = (src & 0x00ff00ff) + (dst_rb & 0x00ff00ff);

                let dst_g = ((dst & 0x0000ff00) * inv_alpha) >> 8;
                let g = (src & 0x0000ff00) + (dst_g & 0x0000ff00);

                pixels[dst_idx] = 0xff000000 | (rb & 0x00ff00ff) | (g & 0x0000ff00);
            }
        }
    }

    unsafe {
        libc::munmap(ptr, pool.size as usize);
    }
}

// 12x19 arrow cursor bitmap. Each row is a string:
//   'B' = black (outline), 'W' = white (fill), '.' = transparent
const CURSOR_W: usize = 12;
const CURSOR_H: usize = 19;
// TODO: we need to eventually implement client-set cursors.
const CURSOR_BITMAP: [&[u8; CURSOR_W]; CURSOR_H] = [
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

const CURSOR_BLACK: u32 = 0xff000000;
const CURSOR_WHITE: u32 = 0xffffffff;

fn blit_cursor(pixels: &mut [u32], width: u32, height: u32, cx: i32, cy: i32) {
    let w = width as i32;
    let h = height as i32;

    for (row_idx, row) in CURSOR_BITMAP.iter().enumerate() {
        let dy = cy + row_idx as i32;
        if dy < 0 || dy >= h {
            continue;
        }
        for (col_idx, &ch) in row.iter().enumerate() {
            let dx = cx + col_idx as i32;
            if dx < 0 || dx >= w {
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
