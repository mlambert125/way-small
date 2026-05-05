//! Software renderer.
//!
//! Composites all surfaces into a single framebuffer using CPU-based
//! rendering. Reads pixel data from client shm pools via mmap, handles
//! subsurface tree traversal, and performs pre-multiplied alpha blending.

use tracing::debug;

use crate::backend::RenderFrame;
use crate::protocol;

const BACKGROUND_COLOR: u32 = 0xff1a_1a2e;

/// Composite all surfaces into a single framebuffer.
pub fn render(state: &protocol::CompositorState, width: u32, height: u32) -> RenderFrame {
    let mut pixels = vec![BACKGROUND_COLOR; (width * height) as usize];

    // Collect top-level surface ids (those without a parent)
    let toplevel_ids: Vec<u32> = state
        .surfaces
        .iter()
        .filter(|(_, s)| s.parent.is_none())
        .map(|(&id, _)| id)
        .collect();

    for surface_id in toplevel_ids {
        blit_surface_tree(state, &mut pixels, width, height, surface_id, 0, 0);
    }

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
    surface_id: u32,
    offset_x: i32,
    offset_y: i32,
) {
    let Some(surface) = state.surfaces.get(&surface_id) else {
        return;
    };

    let children = surface.children.clone();

    // Blit this surface's buffer
    blit_surface_buffer(state, pixels, width, height, surface_id, offset_x, offset_y);

    // Blit children at their positions
    for child_id in children {
        let Some(child) = state.surfaces.get(&child_id) else {
            continue;
        };
        let (cx, cy) = child.subsurface_position;
        blit_surface_tree(
            state,
            pixels,
            width,
            height,
            child_id,
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
    surface_id: u32,
    offset_x: i32,
    offset_y: i32,
) {
    let Some(surface) = state.surfaces.get(&surface_id) else {
        return;
    };
    let Some(buffer_id) = surface.buffer_id else {
        return;
    };
    let Some(shm_buffer) = state.shm_buffers.get(&buffer_id) else {
        return;
    };
    let Some(pool) = state.shm_pools.get(&shm_buffer.pool_id) else {
        return;
    };

    // Validate the fd is still valid and large enough
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(pool.fd, &mut stat) } != 0 {
        debug!("Pool fd {} is invalid for surface {}", pool.fd, surface_id);
        return;
    }
    let actual_size = stat.st_size as u64;
    let buf_end = shm_buffer.offset as u64
        + (shm_buffer.height as u64 - 1) * shm_buffer.stride as u64
        + shm_buffer.width as u64 * 4;
    if buf_end > actual_size {
        debug!(
            "Buffer exceeds actual file size: end={} file_size={} pool_size={} surface={}",
            buf_end, actual_size, pool.size, surface_id
        );
        return;
    }

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
        debug!("Failed to mmap pool for surface {}", surface_id);
        return;
    }

    let buf_w = shm_buffer.width as usize;
    let buf_h = shm_buffer.height as usize;
    let stride = shm_buffer.stride as usize;
    let buf_offset = shm_buffer.offset as usize;
    let dst_w = width as usize;
    let dst_h = height as usize;

    for y in 0..buf_h {
        let dst_y = offset_y as isize + y as isize;
        if dst_y < 0 || dst_y >= dst_h as isize {
            continue;
        }
        let src_row = unsafe {
            std::slice::from_raw_parts((ptr as *const u8).add(buf_offset + y * stride), buf_w * 4)
        };
        let dst_row_start = dst_y as usize * dst_w;

        for x in 0..buf_w {
            let dst_x = offset_x as isize + x as isize;
            if dst_x < 0 || dst_x >= dst_w as isize {
                continue;
            }
            let src = u32::from_le_bytes([
                src_row[x * 4],
                src_row[x * 4 + 1],
                src_row[x * 4 + 2],
                src_row[x * 4 + 3],
            ]);
            let alpha = (src >> 24) & 0xff;
            let dst_idx = dst_row_start + dst_x as usize;
            if alpha == 255 {
                pixels[dst_idx] = src;
            } else if alpha > 0 {
                let dst = pixels[dst_idx];
                let inv_alpha = 255 - alpha;
                let dst_rb = ((dst & 0x00ff00ff) * inv_alpha) >> 8;
                let dst_g = ((dst & 0x0000ff00) * inv_alpha) >> 8;
                let rb = (src & 0x00ff00ff) + (dst_rb & 0x00ff00ff);
                let g = (src & 0x0000ff00) + (dst_g & 0x0000ff00);
                pixels[dst_idx] = 0xff000000 | (rb & 0x00ff00ff) | (g & 0x0000ff00);
            }
        }
    }

    unsafe {
        libc::munmap(ptr, pool.size as usize);
    }
}
