//! Global compositor state.
//!
//! CompositorState holds everything shared across all clients: the client
//! collection, shm pools, buffers, surfaces, and (eventually) outputs, etc.

use std::collections::{HashMap, HashSet};
use std::os::unix::io::RawFd;

use super::client::Clients;

/// A (client_id, object_id) pair that uniquely identifies a Wayland object across all clients.
pub type ClientObjectId = (u32, u32);

/// Tracked state for a wl_shm_pool.
pub struct ShmPool {
    pub client_id: u32,
    pub fd: RawFd,
    pub size: u32,
    /// Cached mmap pointer for the pool. May be null if mmap failed.
    pub map_ptr: *mut libc::c_void,
}

// SAFETY: map_ptr is only accessed from the single compositor task.
unsafe impl Send for ShmPool {}

impl std::fmt::Debug for ShmPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmPool")
            .field("client_id", &self.client_id)
            .field("fd", &self.fd)
            .field("size", &self.size)
            .finish()
    }
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
    /// Whether attach was called since the last commit.
    pub buffer_attached: bool,
    pub buffer_id: Option<u32>,
    pub damage: Vec<(i32, i32, i32, i32)>,
    pub frame_callback: Option<u32>,
    pub presentation_feedbacks: Vec<u32>,
}

/// Committed surface state.
#[derive(Debug)]
pub struct Surface {
    // The wayland client that owns this surface.
    pub client_id: u32,
    // The currently attached buffer, if any.
    pub buffer_id: Option<u32>,
    // The callback to trigger on the next frame, if any.
    pub frame_callback: Option<u32>,
    // Committed presentation feedback objects awaiting the next present.
    pub presentation_feedbacks: Vec<u32>,
    // Pending state from the most recent commit. This is cleared when the compositor
    // applies the commit, but we keep it around here for debugging purposes.
    pub pending: SurfacePending,
    /// If this is a subsurface, its parent surface id.
    pub parent: Option<u32>,
    /// Child subsurface ids in z-order (bottom to top).
    pub children: Vec<u32>,
    /// Position relative to parent (for subsurfaces).
    pub subsurface_position: (i32, i32),
    /// Whether this subsurface commits in sync with its parent.
    pub subsurface_sync: bool,
    /// Position of this surface in global compositor coordinates (for toplevels).
    pub position: (i32, i32),
}

/// Viewport state for wp_viewporter (crop + scale).
#[derive(Debug)]
pub struct ViewportState {
    pub client_id: u32,
    pub surface_id: u32,
    /// Committed source crop (x, y, width, height) in buffer coordinates.
    pub source: Option<(f64, f64, f64, f64)>,
    /// Committed destination size (width, height) in surface coordinates.
    pub destination: Option<(i32, i32)>,
    /// Pending source, applied on next commit. outer Option = "changed", inner = value.
    pub pending_source: Option<Option<(f64, f64, f64, f64)>>,
    /// Pending destination, applied on next commit.
    pub pending_destination: Option<Option<(i32, i32)>>,
}

/// A region: a set of rectangles (adds minus subtracts).
#[derive(Debug, Default)]
pub struct Region {
    // The wayland client that owns this region.
    pub client_id: u32,
    // Rectangles that make up this region
    pub rects: Vec<(i32, i32, i32, i32)>,
    // Negative rectangles (subtracted) that make up this region.
    pub subtracts: Vec<(i32, i32, i32, i32)>,
}

/// XDG surface role (toplevel or popup).
#[derive(Debug)]
pub enum XdgRole {
    Toplevel(u32),
    Popup(u32),
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

/// State for an xdg_popup.
#[derive(Debug)]
pub struct XdgPopupState {
    pub client_id: u32,
    pub xdg_surface_id: u32,
    /// The parent xdg_surface this popup is positioned relative to.
    pub parent_xdg_surface_id: u32,
    /// Computed position relative to parent surface (from positioner).
    pub x: i32,
    pub y: i32,
    /// Size from positioner.
    pub width: i32,
    pub height: i32,
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
    pub shm_pools: HashMap<ClientObjectId, ShmPool>,
    pub shm_buffers: HashMap<ClientObjectId, ShmBuffer>,
    pub surfaces: HashMap<ClientObjectId, Surface>,
    pub regions: HashMap<ClientObjectId, Region>,
    pub xdg_surfaces: HashMap<ClientObjectId, XdgSurfaceState>,
    pub xdg_toplevels: HashMap<ClientObjectId, XdgToplevelState>,
    pub xdg_popups: HashMap<ClientObjectId, XdgPopupState>,
    pub xdg_positioners: HashMap<ClientObjectId, XdgPositionerState>,
    pub seat: SeatState,
    pub output: Option<OutputState>,
    pub pointers: Vec<PointerBinding>,
    pub keyboards: Vec<KeyboardBinding>,
    pub focused_surface: Option<ClientObjectId>,
    pub cursor_x: f64,
    pub cursor_y: f64,
    /// Currently pressed evdev keycodes (for wl_keyboard.enter keys array).
    pub pressed_keys: HashSet<u32>,
    /// Maps wl_subsurface (client_id, object_id) -> the wl_surface object id it controls.
    pub subsurface_map: HashMap<ClientObjectId, u32>,
    /// wp_viewport objects keyed by (client_id, viewport_object_id).
    pub viewports: HashMap<ClientObjectId, ViewportState>,
    /// Reverse map: (client_id, surface_id) -> viewport_object_id.
    pub surface_viewport: HashMap<ClientObjectId, u32>,
    /// Buffers to release on the next render (old buffers replaced by commit).
    pub buffers_pending_release: Vec<ClientObjectId>,
    /// Next position for cascading toplevel placement.
    pub next_toplevel_position: (i32, i32),
    /// Toplevel surface draw order, bottom to top.
    pub surface_stack: Vec<ClientObjectId>,
    /// Whether visual state has changed and a re-render is needed.
    pub dirty: bool,
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
            xdg_popups: HashMap::new(),
            xdg_positioners: HashMap::new(),
            seat: SeatState::default(),
            output: None,
            pointers: Vec::new(),
            keyboards: Vec::new(),
            focused_surface: None,
            cursor_x: 0.0,
            cursor_y: 0.0,
            pressed_keys: HashSet::new(),
            subsurface_map: HashMap::new(),
            viewports: HashMap::new(),
            surface_viewport: HashMap::new(),
            buffers_pending_release: Vec::new(),
            next_toplevel_position: (50, 50),
            surface_stack: Vec::new(),
            dirty: true,
        }
    }

    pub fn register_shm_pool(&mut self, client_id: u32, pool_id: u32, fd: RawFd, size: u32) {
        let map_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        let map_ptr = if map_ptr == libc::MAP_FAILED {
            std::ptr::null_mut()
        } else {
            map_ptr
        };
        self.shm_pools.insert(
            (client_id, pool_id),
            ShmPool {
                client_id,
                fd,
                size,
                map_ptr,
            },
        );
    }

    pub fn destroy_shm_pool(&mut self, client_id: u32, pool_id: u32) {
        if let Some(pool) = self.shm_pools.remove(&(client_id, pool_id)) {
            if !pool.map_ptr.is_null() {
                unsafe { libc::munmap(pool.map_ptr, pool.size as usize) };
            }
            unsafe { libc::close(pool.fd) };
        }
    }

    pub fn resize_shm_pool(&mut self, client_id: u32, pool_id: u32, new_size: u32) {
        if let Some(pool) = self.shm_pools.get_mut(&(client_id, pool_id)) {
            // Unmap old mapping
            if !pool.map_ptr.is_null() {
                unsafe { libc::munmap(pool.map_ptr, pool.size as usize) };
            }
            pool.size = new_size;
            // Remap with new size
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    new_size as usize,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    pool.fd,
                    0,
                )
            };
            pool.map_ptr = if ptr == libc::MAP_FAILED {
                std::ptr::null_mut()
            } else {
                ptr
            };
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register_buffer(
        &mut self,
        client_id: u32,
        buffer_id: u32,
        pool_id: u32,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    ) {
        self.shm_buffers.insert(
            (client_id, buffer_id),
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

    pub fn destroy_buffer(&mut self, client_id: u32, buffer_id: u32) {
        self.shm_buffers.remove(&(client_id, buffer_id));
    }

    pub fn create_surface(&mut self, client_id: u32, surface_id: u32) {
        self.surfaces.insert(
            (client_id, surface_id),
            Surface {
                client_id,
                buffer_id: None,
                frame_callback: None,
                presentation_feedbacks: Vec::new(),
                pending: SurfacePending::default(),
                parent: None,
                children: Vec::new(),
                subsurface_position: (0, 0),
                subsurface_sync: true,
                position: (0, 0),
            },
        );
    }

    pub fn destroy_surface(&mut self, client_id: u32, surface_id: u32) {
        self.surfaces.remove(&(client_id, surface_id));
    }

    pub fn create_region(&mut self, client_id: u32, region_id: u32) {
        self.regions.insert(
            (client_id, region_id),
            Region {
                client_id,
                ..Default::default()
            },
        );
    }

    pub fn destroy_region(&mut self, client_id: u32, region_id: u32) {
        self.regions.remove(&(client_id, region_id));
    }

    pub fn create_xdg_surface(&mut self, client_id: u32, xdg_surface_id: u32, wl_surface_id: u32) {
        self.xdg_surfaces.insert(
            (client_id, xdg_surface_id),
            XdgSurfaceState {
                client_id,
                wl_surface_id,
                role: None,
                configured: false,
                geometry: None,
            },
        );
    }

    pub fn destroy_xdg_surface(&mut self, client_id: u32, xdg_surface_id: u32) {
        self.xdg_surfaces.remove(&(client_id, xdg_surface_id));
    }

    pub fn create_xdg_toplevel(&mut self, client_id: u32, toplevel_id: u32, xdg_surface_id: u32) {
        self.xdg_toplevels.insert(
            (client_id, toplevel_id),
            XdgToplevelState {
                client_id,
                xdg_surface_id,
                title: None,
                app_id: None,
            },
        );
        if let Some(xdg_surface) = self.xdg_surfaces.get_mut(&(client_id, xdg_surface_id)) {
            xdg_surface.role = Some(XdgRole::Toplevel(toplevel_id));
            // Assign cascade position to the toplevel's wl_surface
            let wl_surface_id = xdg_surface.wl_surface_id;
            if let Some(surface) = self.surfaces.get_mut(&(client_id, wl_surface_id)) {
                surface.position = self.next_toplevel_position;
            }
            self.next_toplevel_position.0 += 50;
            self.next_toplevel_position.1 += 50;
            // Add to top of surface stack
            let surface_key = (client_id, wl_surface_id);
            self.surface_stack.retain(|k| *k != surface_key);
            self.surface_stack.push(surface_key);
            self.dirty = true;
        }
    }

    pub fn destroy_xdg_toplevel(&mut self, client_id: u32, toplevel_id: u32) {
        if let Some(toplevel) = self.xdg_toplevels.remove(&(client_id, toplevel_id)) {
            // Remove the associated wl_surface from the stack and clear focus
            if let Some(xdg_surface) = self.xdg_surfaces.get(&(client_id, toplevel.xdg_surface_id)) {
                let surface_key = (client_id, xdg_surface.wl_surface_id);
                self.surface_stack.retain(|k| *k != surface_key);
                if self.focused_surface == Some(surface_key) {
                    self.focused_surface = None;
                }
                self.dirty = true;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_xdg_popup(
        &mut self,
        client_id: u32,
        popup_id: u32,
        xdg_surface_id: u32,
        parent_xdg_surface_id: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        self.xdg_popups.insert(
            (client_id, popup_id),
            XdgPopupState {
                client_id,
                xdg_surface_id,
                parent_xdg_surface_id,
                x,
                y,
                width,
                height,
            },
        );
        if let Some(xdg_surface) = self.xdg_surfaces.get_mut(&(client_id, xdg_surface_id)) {
            xdg_surface.role = Some(XdgRole::Popup(popup_id));
        }
    }

    pub fn destroy_xdg_popup(&mut self, client_id: u32, popup_id: u32) {
        self.xdg_popups.remove(&(client_id, popup_id));
    }

    pub fn create_xdg_positioner(&mut self, client_id: u32, positioner_id: u32) {
        self.xdg_positioners.insert(
            (client_id, positioner_id),
            XdgPositionerState {
                client_id,
                ..Default::default()
            },
        );
    }

    pub fn destroy_xdg_positioner(&mut self, client_id: u32, positioner_id: u32) {
        self.xdg_positioners.remove(&(client_id, positioner_id));
    }

    pub fn create_viewport(&mut self, client_id: u32, viewport_id: u32, surface_id: u32) {
        self.viewports.insert(
            (client_id, viewport_id),
            ViewportState {
                client_id,
                surface_id,
                source: None,
                destination: None,
                pending_source: None,
                pending_destination: None,
            },
        );
        self.surface_viewport
            .insert((client_id, surface_id), viewport_id);
    }

    pub fn destroy_viewport(&mut self, client_id: u32, viewport_id: u32) {
        if let Some(vp) = self.viewports.remove(&(client_id, viewport_id)) {
            self.surface_viewport.remove(&(client_id, vp.surface_id));
        }
    }

    /// Remove all pools, buffers, and surfaces belonging to a disconnecting client.
    pub fn remove_client_resources(&mut self, client_id: u32) {
        let pool_ids: Vec<u32> = self
            .shm_pools
            .iter()
            .filter(|(_, p)| p.client_id == client_id)
            .map(|(&(_, obj_id), _)| obj_id)
            .collect();
        for id in pool_ids {
            self.destroy_shm_pool(client_id, id);
        }
        self.shm_buffers.retain(|_, b| b.client_id != client_id);
        self.surfaces.retain(|_, s| s.client_id != client_id);
        self.regions.retain(|_, r| r.client_id != client_id);
        self.xdg_toplevels.retain(|_, t| t.client_id != client_id);
        self.xdg_popups.retain(|_, p| p.client_id != client_id);
        self.xdg_surfaces.retain(|_, s| s.client_id != client_id);
        self.xdg_positioners.retain(|_, p| p.client_id != client_id);
        self.viewports.retain(|_, v| v.client_id != client_id);
        self.surface_viewport.retain(|(cid, _), _| *cid != client_id);
        self.pointers.retain(|p| p.client_id != client_id);
        self.keyboards.retain(|k| k.client_id != client_id);
        self.subsurface_map.retain(|(cid, _), _| *cid != client_id);
        self.surface_stack.retain(|(cid, _)| *cid != client_id);
        self.buffers_pending_release
            .retain(|(cid, _)| *cid != client_id);
        // Clear focus if it pointed to a surface owned by this client
        if let Some((cid, _)) = self.focused_surface
            && cid == client_id
        {
            self.focused_surface = None;
        }
    }
}
