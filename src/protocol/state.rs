//! Global compositor state.
//!
//! CompositorState holds everything shared across all clients: the client
//! collection, shm pools, buffers, surfaces, and (eventually) outputs, etc.

use std::collections::HashMap;
use std::os::unix::io::RawFd;

use super::client::Clients;

/// Tracked state for a wl_shm_pool.
#[derive(Debug)]
pub struct ShmPool {
    pub client_id: u32,
    pub fd: RawFd,
    pub size: u32,
}

/// Tracked state for a wl_buffer backed by a shm pool.
#[derive(Debug)]
#[allow(dead_code)]
pub struct ShmBuffer {
    pub client_id: u32,
    pub pool_id: u32,
    pub offset: i32,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: u32,
}

/// Pending state for a surface, applied on commit.
#[derive(Debug, Default, Clone)]
pub struct SurfacePending {
    pub buffer_id: Option<u32>,
    pub damage: Vec<(i32, i32, i32, i32)>,
    pub frame_callback: Option<u32>,
}

/// Committed surface state.
#[derive(Debug)]
pub struct Surface {
    pub client_id: u32,
    pub buffer_id: Option<u32>,
    pub frame_callback: Option<u32>,
    pub pending: SurfacePending,
    /// If this is a subsurface, its parent surface id.
    pub parent: Option<u32>,
    /// Child subsurface ids in z-order (bottom to top).
    pub children: Vec<u32>,
    /// Position relative to parent (for subsurfaces).
    pub subsurface_position: (i32, i32),
    /// Whether this subsurface commits in sync with its parent.
    pub subsurface_sync: bool,
}

/// A region: a set of rectangles (adds minus subtracts).
#[derive(Debug, Default)]
pub struct Region {
    pub client_id: u32,
    pub rects: Vec<(i32, i32, i32, i32)>,
    pub subtracts: Vec<(i32, i32, i32, i32)>,
}

/// XDG surface role (toplevel or popup).
#[derive(Debug)]
pub enum XdgRole {
    #[allow(dead_code)]
    Toplevel(u32), // toplevel object id
}

/// State for an xdg_surface (wraps a wl_surface with window semantics).
#[derive(Debug)]
#[allow(dead_code)]
pub struct XdgSurfaceState {
    pub client_id: u32,
    pub wl_surface_id: u32,
    pub role: Option<XdgRole>,
    pub configured: bool,
    /// Window geometry (x, y, width, height) — the "meaningful" content area
    /// excluding shadows/decorations. None until the client sets it.
    pub geometry: Option<(i32, i32, i32, i32)>,
}

/// State for an xdg_toplevel.
#[derive(Debug)]
#[allow(dead_code)]
pub struct XdgToplevelState {
    pub client_id: u32,
    pub xdg_surface_id: u32,
    pub title: Option<String>,
    pub app_id: Option<String>,
}

/// State for an xdg_positioner (used to position popup surfaces).
#[derive(Debug, Default, Clone)]
pub struct XdgPositionerState {
    pub client_id: u32,
    pub width: i32,
    pub height: i32,
    pub anchor_rect: (i32, i32, i32, i32), // x, y, width, height
    pub anchor: u32,
    pub gravity: u32,
    pub offset: (i32, i32),
    pub constraint_adjustment: u32,
}

/// A client's bound pointer object.
#[derive(Debug, Clone)]
pub struct PointerBinding {
    pub client_id: u32,
    pub object_id: u32,
}

/// A client's bound keyboard object.
#[derive(Debug, Clone)]
pub struct KeyboardBinding {
    pub client_id: u32,
    pub object_id: u32,
}

/// Seat capabilities reported by the backend.
#[derive(Debug, Default)]
pub struct SeatState {
    pub has_pointer: bool,
    pub has_keyboard: bool,
}

/// Output information reported by the backend.
#[derive(Debug)]
pub struct OutputState {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

/// Global compositor state shared across all clients.
pub struct CompositorState {
    pub clients: Clients,
    pub shm_pools: HashMap<u32, ShmPool>,
    pub shm_buffers: HashMap<u32, ShmBuffer>,
    pub surfaces: HashMap<u32, Surface>,
    pub regions: HashMap<u32, Region>,
    pub xdg_surfaces: HashMap<u32, XdgSurfaceState>,
    pub xdg_toplevels: HashMap<u32, XdgToplevelState>,
    pub xdg_positioners: HashMap<u32, XdgPositionerState>,
    pub seat: SeatState,
    pub output: Option<OutputState>,
    pub pointers: Vec<PointerBinding>,
    pub keyboards: Vec<KeyboardBinding>,
    /// The surface that currently has pointer/keyboard focus.
    pub focused_surface: Option<u32>,
    /// Maps wl_subsurface object id -> the wl_surface id it controls.
    pub subsurface_map: HashMap<u32, u32>,
}

impl CompositorState {
    pub fn new() -> Self {
        Self {
            clients: Clients::new(),
            shm_pools: HashMap::new(),
            shm_buffers: HashMap::new(),
            surfaces: HashMap::new(),
            regions: HashMap::new(),
            xdg_surfaces: HashMap::new(),
            xdg_toplevels: HashMap::new(),
            xdg_positioners: HashMap::new(),
            seat: SeatState::default(),
            output: None,
            pointers: Vec::new(),
            keyboards: Vec::new(),
            focused_surface: None,
            subsurface_map: HashMap::new(),
        }
    }

    pub fn register_shm_pool(&mut self, pool_id: u32, client_id: u32, fd: RawFd, size: u32) {
        self.shm_pools.insert(
            pool_id,
            ShmPool {
                client_id,
                fd,
                size,
            },
        );
    }

    pub fn destroy_shm_pool(&mut self, pool_id: u32) {
        if let Some(pool) = self.shm_pools.remove(&pool_id) {
            unsafe { libc::close(pool.fd) };
        }
    }

    pub fn resize_shm_pool(&mut self, pool_id: u32, new_size: u32) {
        if let Some(pool) = self.shm_pools.get_mut(&pool_id) {
            pool.size = new_size;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register_buffer(
        &mut self,
        buffer_id: u32,
        client_id: u32,
        pool_id: u32,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    ) {
        self.shm_buffers.insert(
            buffer_id,
            ShmBuffer {
                client_id,
                pool_id,
                offset,
                width,
                height,
                stride,
                format,
            },
        );
    }

    pub fn destroy_buffer(&mut self, buffer_id: u32) {
        self.shm_buffers.remove(&buffer_id);
    }

    pub fn create_surface(&mut self, surface_id: u32, client_id: u32) {
        self.surfaces.insert(
            surface_id,
            Surface {
                client_id,
                buffer_id: None,
                frame_callback: None,
                pending: SurfacePending::default(),
                parent: None,
                children: Vec::new(),
                subsurface_position: (0, 0),
                subsurface_sync: true,
            },
        );
    }

    pub fn destroy_surface(&mut self, surface_id: u32) {
        self.surfaces.remove(&surface_id);
    }

    pub fn create_region(&mut self, region_id: u32, client_id: u32) {
        self.regions.insert(
            region_id,
            Region {
                client_id,
                ..Default::default()
            },
        );
    }

    pub fn destroy_region(&mut self, region_id: u32) {
        self.regions.remove(&region_id);
    }

    pub fn create_xdg_surface(&mut self, xdg_surface_id: u32, client_id: u32, wl_surface_id: u32) {
        self.xdg_surfaces.insert(
            xdg_surface_id,
            XdgSurfaceState {
                client_id,
                wl_surface_id,
                role: None,
                configured: false,
                geometry: None,
            },
        );
    }

    pub fn destroy_xdg_surface(&mut self, xdg_surface_id: u32) {
        self.xdg_surfaces.remove(&xdg_surface_id);
    }

    pub fn create_xdg_toplevel(&mut self, toplevel_id: u32, client_id: u32, xdg_surface_id: u32) {
        self.xdg_toplevels.insert(
            toplevel_id,
            XdgToplevelState {
                client_id,
                xdg_surface_id,
                title: None,
                app_id: None,
            },
        );
        if let Some(xdg_surface) = self.xdg_surfaces.get_mut(&xdg_surface_id) {
            xdg_surface.role = Some(XdgRole::Toplevel(toplevel_id));
        }
    }

    pub fn destroy_xdg_toplevel(&mut self, toplevel_id: u32) {
        self.xdg_toplevels.remove(&toplevel_id);
    }

    pub fn create_xdg_positioner(&mut self, positioner_id: u32, client_id: u32) {
        self.xdg_positioners.insert(
            positioner_id,
            XdgPositionerState {
                client_id,
                ..Default::default()
            },
        );
    }

    pub fn destroy_xdg_positioner(&mut self, positioner_id: u32) {
        self.xdg_positioners.remove(&positioner_id);
    }

    /// Remove all pools, buffers, and surfaces belonging to a disconnecting client.
    pub fn remove_client_resources(&mut self, client_id: u32) {
        let pool_ids: Vec<u32> = self
            .shm_pools
            .iter()
            .filter(|(_, p)| p.client_id == client_id)
            .map(|(&id, _)| id)
            .collect();
        for id in pool_ids {
            self.destroy_shm_pool(id);
        }
        self.shm_buffers.retain(|_, b| b.client_id != client_id);
        self.surfaces.retain(|_, s| s.client_id != client_id);
        self.regions.retain(|_, r| r.client_id != client_id);
        self.xdg_toplevels.retain(|_, t| t.client_id != client_id);
        self.xdg_surfaces.retain(|_, s| s.client_id != client_id);
        self.xdg_positioners.retain(|_, p| p.client_id != client_id);
        self.pointers.retain(|p| p.client_id != client_id);
        self.keyboards.retain(|k| k.client_id != client_id);
        self.subsurface_map.retain(|_, surface_id| self.surfaces.contains_key(surface_id));
        // Clear focus if it pointed to a surface owned by this client
        if let Some(surface_id) = self.focused_surface
            && !self.surfaces.contains_key(&surface_id)
        {
            self.focused_surface = None;
        }
    }
}
