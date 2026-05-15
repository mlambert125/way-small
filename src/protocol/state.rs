//! Global compositor state.
//!
//! `CompositorState` holds everything shared across all clients: the client
//! collection, shm pools, buffers, surfaces, and (eventually) outputs, etc.

use std::collections::{HashMap, HashSet};
use std::os::unix::io::RawFd;

use super::client::Clients;

pub const OUTPUT_MODE_CURRENT: u32 = 0x1;
pub const OUTPUT_MODE_PREFERRED: u32 = 0x2;

pub type ClientObjectId = (u32, u32);

pub struct ShmPool {
    pub client_id: u32,
    pub fd: RawFd,
    pub size: u32,
    pub map_ptr: *mut libc::c_void,
    /// True after `wl_shm_pool.destroy` — the pool will be freed once no
    /// buffers reference it.
    pub dead: bool,
}

unsafe impl Send for ShmPool {}

impl std::fmt::Debug for ShmPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmPool")
            .field("client_id", &self.client_id)
            .field("fd", &self.fd)
            .field("size", &self.size)
            .field("map_ptr", &self.map_ptr)
            .field("dead", &self.dead)
            .finish()
    }
}

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

#[derive(Debug, Default, Clone)]
pub struct SurfacePending {
    pub buffer_attached: bool,
    pub buffer_id: Option<u32>,
    pub damage: Vec<(i32, i32, i32, i32)>,
    pub frame_callback: Option<u32>,
    pub presentation_feedbacks: Vec<u32>,
}

#[derive(Debug)]
pub struct Surface {
    pub client_id: u32,
    pub buffer_id: Option<u32>,
    pub frame_callback: Option<u32>,
    pub presentation_feedbacks: Vec<u32>,
    pub pending: SurfacePending,
    pub parent: Option<u32>,
    pub children: Vec<u32>,
    pub subsurface_position: (i32, i32),
    pub subsurface_sync: bool,
    pub position: (i32, i32),
}

#[derive(Debug)]
pub struct ViewportState {
    pub client_id: u32,
    pub surface_id: u32,
    pub source: Option<(f64, f64, f64, f64)>,
    pub destination: Option<(i32, i32)>,
    pub pending_source: Option<(f64, f64, f64, f64)>,
    pub pending_destination: Option<(i32, i32)>,
}

#[derive(Debug, Default)]
pub struct Region {
    pub client_id: u32,
    pub rects: Vec<(i32, i32, i32, i32)>,
    pub subtracts: Vec<(i32, i32, i32, i32)>,
}

#[derive(Debug)]
pub enum XdgRole {
    Toplevel(u32),
    Popup(u32),
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct XdgSurfaceState {
    pub client_id: u32,
    pub wl_surface_id: u32,
    pub role: Option<XdgRole>,
    pub configured: bool,
    pub geometry: Option<(i32, i32, i32, i32)>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct XdgToplevelState {
    pub client_id: u32,
    pub xdg_surface_id: u32,
    pub title: Option<String>,
    pub app_id: Option<String>,
}

#[derive(Debug)]
pub struct XdgPopupState {
    pub client_id: u32,
    pub xdg_surface_id: u32,
    pub parent_xdg_surface_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Default, Clone)]
pub struct XdgPositionerState {
    pub client_id: u32,
    pub width: i32,
    pub height: i32,
    pub anchor_rect: (i32, i32, i32, i32),
    pub anchor: u32,
    pub gravity: u32,
    pub offset: (i32, i32),
    pub constraint_adjustment: u32,
}

#[derive(Debug, Clone)]
pub struct PointerBinding {
    pub client_id: u32,
    pub object_id: u32,
}

#[derive(Debug, Clone)]
pub struct KeyboardBinding {
    pub client_id: u32,
    pub object_id: u32,
}

#[derive(Debug, Default)]
pub struct SeatState {
    pub has_pointer: bool,
    pub has_keyboard: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum OutputTransform {
    Normal = 0,
    Rotate90 = 1,
    Rotate180 = 2,
    Rotate270 = 3,
    Flipped = 4,
    Flipped90 = 5,
    Flipped180 = 6,
    Flipped270 = 7,
}

#[derive(Debug, Clone, Copy)]
pub enum OutputSubpixel {
    Unknown = 0,
    None = 1,
    HorizontalRgb = 2,
    HorizontalBgr = 3,
    VerticalRgb = 4,
    VerticalBgr = 5,
}

#[derive(Debug, Clone)]
pub struct OutputGeometry {
    pub x: i32,
    pub y: i32,
    pub physical_width: i32,
    pub physical_height: i32,
    pub subpixel: OutputSubpixel,
    pub make: String,
    pub model: String,
    pub transform: OutputTransform,
}

#[derive(Debug, Clone)]
pub struct OutputMode {
    pub flags: u32,
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(pub u32);

#[derive(Debug, Clone)]
pub struct Output {
    pub id: OutputId,
    pub geometry: OutputGeometry,
    pub modes: Vec<OutputMode>,
    pub scale: i32,
    pub name: String,
    pub description: String,
}

pub struct DefaultCursor {
    pub pixels: Vec<u32>,
    pub width: i32,
    pub height: i32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
}

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
    pub outputs: Vec<Output>,
    /// Maps `OutputId` -> `wl_registry` global name for dynamic output globals.
    pub output_global_names: HashMap<OutputId, u32>,
    /// Maps (`client_id`, `wl_output_object_id`) -> which output the binding refers to.
    pub output_bindings: HashMap<ClientObjectId, OutputId>,
    /// Counter for assigning unique `wl_registry` global names to new outputs.
    pub next_global_number: u32,
    pub pointers: Vec<PointerBinding>,
    pub keyboards: Vec<KeyboardBinding>,
    pub focused_surface: Option<ClientObjectId>,
    /// The specific surface (possibly a subsurface) currently under the pointer.
    /// Used for delivering pointer enter/leave/motion/button to the correct surface.
    pub pointer_surface: Option<ClientObjectId>,
    pub cursor_x: f64,
    pub cursor_y: f64,
    /// Currently pressed evdev keycodes (for `wl_keyboard.enter` keys array).
    pub pressed_keys: HashSet<u32>,
    /// Maps `wl_subsurface` (`client_id`, `object_id`) -> the `wl_surface` object id it controls.
    pub subsurface_map: HashMap<ClientObjectId, u32>,
    /// `wp_viewport` objects keyed by (`client_id`, `viewport_object_id`).
    pub viewports: HashMap<ClientObjectId, ViewportState>,
    /// Reverse map: (`client_id`, `surface_id`) -> `viewport_object_id`.
    pub surface_viewport: HashMap<ClientObjectId, u32>,
    /// Buffers to release on the next render (old buffers replaced by commit).
    pub buffers_pending_release: Vec<ClientObjectId>,
    /// Next position for cascading toplevel placement.
    pub next_toplevel_position: (i32, i32),
    /// Toplevel surface draw order, bottom to top.
    pub surface_stack: Vec<ClientObjectId>,
    /// Whether visual state has changed and a re-render is needed.
    pub dirty: bool,
    /// Per-client cursor: `client_id` -> `Some((surface_id, hotspot_x, hotspot_y))` or `None` (hidden).
    pub cursor_surfaces: HashMap<u32, Option<(u32, i32, i32)>>,
    /// Most recent `wl_pointer.enter` serial per client, for `set_cursor` validation.
    pub pointer_enter_serial: HashMap<u32, u32>,
    /// Surfaces with the cursor role (permanent, prevents other role assignment).
    pub cursor_role_surfaces: HashSet<ClientObjectId>,
    /// Pre-loaded cursor from the system cursor theme, used when no client cursor is set.
    pub default_cursor: Option<DefaultCursor>,
    /// Stack of popup (`client_id`, `xdg_popup_id`) that have called `grab`, newest on top.
    /// When the user clicks outside the topmost grabbed popup, it is dismissed.
    pub grabbed_popups: Vec<ClientObjectId>,
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
            outputs: Vec::new(),
            output_global_names: HashMap::new(),
            output_bindings: HashMap::new(),
            next_global_number: u32::try_from(super::GLOBALS.len()).unwrap_or(1),
            pointers: Vec::new(),
            keyboards: Vec::new(),
            focused_surface: None,
            pointer_surface: None,
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
            cursor_surfaces: HashMap::new(),
            pointer_enter_serial: HashMap::new(),
            cursor_role_surfaces: HashSet::new(),
            default_cursor: None,
            grabbed_popups: Vec::new(),
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
                dead: false,
            },
        );
    }

    pub fn destroy_shm_pool(&mut self, client_id: u32, pool_id: u32) {
        if let Some(pool) = self.shm_pools.get_mut(&(client_id, pool_id)) {
            pool.dead = true;
        }
        self.try_cleanup_pool(client_id, pool_id);
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
        let pool_id = self
            .shm_buffers
            .get(&(client_id, buffer_id))
            .map(|b| b.pool_id);
        self.shm_buffers.remove(&(client_id, buffer_id));
        if let Some(pool_id) = pool_id {
            self.try_cleanup_pool(client_id, pool_id);
        }
    }

    /// Free pool resources if the pool has been destroyed and no buffers
    /// still reference it.
    fn try_cleanup_pool(&mut self, client_id: u32, pool_id: u32) {
        let pool_is_dead = self
            .shm_pools
            .get(&(client_id, pool_id))
            .is_some_and(|p| p.dead);
        if !pool_is_dead {
            return;
        }
        let has_buffers = self
            .shm_buffers
            .values()
            .any(|b| b.client_id == client_id && b.pool_id == pool_id);
        if !has_buffers && let Some(pool) = self.shm_pools.remove(&(client_id, pool_id)) {
            if !pool.map_ptr.is_null() {
                unsafe { libc::munmap(pool.map_ptr, pool.size as usize) };
            }
            unsafe { libc::close(pool.fd) };
        }
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
            if let Some(xdg_surface) = self.xdg_surfaces.get(&(client_id, toplevel.xdg_surface_id))
            {
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
        self.surface_viewport
            .retain(|(cid, _), _| *cid != client_id);
        self.pointers.retain(|p| p.client_id != client_id);
        self.keyboards.retain(|k| k.client_id != client_id);
        self.output_bindings.retain(|(cid, _), _| *cid != client_id);
        self.cursor_surfaces.remove(&client_id);
        self.pointer_enter_serial.remove(&client_id);
        self.cursor_role_surfaces
            .retain(|(cid, _)| *cid != client_id);
        self.subsurface_map.retain(|(cid, _), _| *cid != client_id);
        self.grabbed_popups.retain(|(cid, _)| *cid != client_id);
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
