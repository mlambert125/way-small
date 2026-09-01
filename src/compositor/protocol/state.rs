//! Global compositor state.
//!
//! `CompositorState` holds everything shared across all clients: the client
//! collection, shm pools, buffers, surfaces, and (eventually) outputs, etc.

use super::super::workspace::{Workspace, Workspaces};
use super::client::Clients;
use super::wire_utils::f64_to_i32;
use super::wl_pointer;
use crate::shared::{
    BufferGuard, Output, OutputId, PoolMapping, TextureRect, cursor_bounds, output_contains,
};
use std::collections::{HashMap, HashSet};
use std::os::unix::io::RawFd;
use std::sync::Arc;
use strum::FromRepr;

/// A client object-id tuple holding a `client_id` paired with an object id.  This pair
/// is needed because object ids are only unique per client, so the `client_id` is paired
/// to make it unique
pub type ClientObjectId = (u32, u32);

/// A shared memory pool
#[derive(Debug)]
pub struct ShmPool {
    /// Client owning the pool
    pub client_id: u32,
    /// The file descriptor pointing to the shared memory
    pub fd: RawFd,
    /// The size of the shared memory
    pub size: u32,
    /// A live mmap of the shared memory.
    /// `None` if the mapping failed; the pool then renders nothing.
    pub mapping: Option<Arc<PoolMapping>>,
    /// True after `wl_shm_pool.destroy` — the pool will be freed once no
    /// buffers reference it.
    pub dead: bool,
}

/// An individual buffer in some `ShmPool`
#[derive(Debug)]
#[allow(dead_code)]
pub struct ShmBuffer {
    /// Client owning the buffer
    pub client_id: u32,
    /// Pool Id that this buffer points into
    pub pool_id: u32,
    /// Offset into the pool where this buffer begins
    pub offset: i32,
    /// Width of this buffer in pixels
    pub width: i32,
    /// Height of this buffer in pixels
    pub height: i32,
    /// Actual byte length of each row in this buffer (includes padding, etc.)
    pub stride: i32,
    /// Format of this buffer
    pub format: u32,
    /// Identifies the current contents of this buffer.
    ///
    /// Drawn from a counter that never repeats, so a buffer id reused after
    /// destruction cannot collide with the old one, and anything holding a
    /// copy can tell whether it is still current by comparing serials alone.
    pub content_serial: u64,
    /// What changed since the damage was last consumed, in buffer pixels.
    ///
    /// `None` means "assume everything" — the client told us nothing, the
    /// buffer is new, or its mapping moved. Damage is a promise about what did
    /// *not* change, so anything uncertain has to widen to the whole buffer.
    /// `Some` is exact, and an empty `Some` means nothing has changed since it
    /// was last read, which is why the two cannot share a representation.
    pub damage: Option<Vec<TextureRect>>,
}

#[derive(Debug, Default, Clone)]
pub struct SurfacePending {
    pub buffer_attached: bool,
    pub buffer_id: Option<u32>,
    /// `wl_surface.damage` rectangles, in surface-local coordinates.
    pub damage_surface: Vec<TextureRect>,
    /// `wl_surface.damage_buffer` rectangles, already in buffer coordinates.
    pub damage_buffer: Vec<TextureRect>,
    pub frame_callback: Option<u32>,
    pub presentation_feedbacks: Vec<u32>,
    pub input_region: PendingInputRegion,
    /// Pending `wl_surface.set_buffer_scale`.
    pub buffer_scale: Option<i32>,
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
    /// Which parts of the surface accept pointer input, in surface-local
    /// coordinates. `None` is the protocol default: the whole surface does.
    pub input_region: Option<Vec<RegionRect>>,
    /// How many buffer pixels map to one surface-local coordinate. Clients on a
    /// scaled output submit a correspondingly larger buffer, so the surface's
    /// logical size is its buffer size divided by this. Always at least 1.
    pub buffer_scale: i32,
    /// Outputs the client has been told this surface is on, via
    /// `wl_surface.enter`. Diffed each frame so only changes are sent.
    pub entered_outputs: HashSet<OutputId>,
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

/// How a surface's buffer maps onto the screen.
#[derive(Debug, Clone, Copy)]
pub struct BufferMapping {
    /// Source rectangle in buffer pixels: (x, y, width, height).
    pub src: (f64, f64, f64, f64),
    /// Destination size in surface coordinates.
    pub dest_width: i32,
    pub dest_height: i32,
}

/// Which edges of a window an interactive resize is dragging.
///
/// The bit values are `xdg_toplevel.resize_edge`, passed straight through from
/// the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeEdges(pub u32);

impl ResizeEdges {
    pub const TOP: u32 = 1;
    pub const BOTTOM: u32 = 2;
    pub const LEFT: u32 = 4;
    pub const RIGHT: u32 = 8;

    pub fn top(self) -> bool {
        self.0 & Self::TOP != 0
    }
    pub fn bottom(self) -> bool {
        self.0 & Self::BOTTOM != 0
    }
    pub fn left(self) -> bool {
        self.0 & Self::LEFT != 0
    }
    pub fn right(self) -> bool {
        self.0 & Self::RIGHT != 0
    }
}

/// What an interactive grab is doing to the window it holds.
#[derive(Debug, Clone, Copy)]
pub enum GrabKind {
    /// Dragging the window. The offset from pointer to window origin is held
    /// constant, so the window keeps its grip point under the cursor.
    Move { offset_x: i32, offset_y: i32 },
    /// Dragging an edge or corner. Everything is measured from where the drag
    /// began, so the window cannot drift from accumulated rounding.
    Resize {
        edges: ResizeEdges,
        start_pointer: (f64, f64),
        start_position: (i32, i32),
        start_size: (i32, i32),
        /// Last size sent to the client, so an unchanged size sends nothing.
        last_sent: (i32, i32),
    },
}

/// An interactive move or resize the compositor is driving.
///
/// While one is held the compositor owns the pointer: motion and buttons drive
/// the grab instead of reaching the client, which is what "grab" means.
#[derive(Debug, Clone, Copy)]
pub struct PointerGrab {
    /// The toplevel's `wl_surface`, the same key a workspace's stack uses.
    pub surface: ClientObjectId,
    /// The `xdg_toplevel` object, needed to configure a resize.
    pub toplevel: ClientObjectId,
    pub kind: GrabKind,
}

/// Whether a rectangle adds to or subtracts from a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionOp {
    Add,
    Subtract,
}

/// One add/subtract rectangle from a `wl_region`.
#[derive(Debug, Clone, Copy)]
pub struct RegionRect {
    pub op: RegionOp,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl RegionRect {
    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width)
            && y < self.y.saturating_add(self.height)
    }
}

/// Whether a point falls inside the region built from these operations.
///
/// The operations are replayed in order rather than reduced to a set of
/// disjoint rectangles. Order is significant: a rectangle added after a
/// subtraction re-includes the overlapping area, so keeping adds and subtracts
/// in separate lists would get that case wrong. Replaying is exact for point
/// queries, which is all the compositor needs a region for.
pub fn region_contains(rects: &[RegionRect], x: i32, y: i32) -> bool {
    let mut inside = false;
    for rect in rects {
        if rect.contains(x, y) {
            inside = rect.op == RegionOp::Add;
        }
    }
    inside
}

/// A `wl_surface.set_input_region` waiting to be applied at the next commit.
#[derive(Debug, Default, Clone)]
pub enum PendingInputRegion {
    /// The client has not set an input region since the last commit.
    #[default]
    Unchanged,
    /// Reset to the protocol default, where the whole surface accepts input.
    /// This is what the null region argument means.
    Infinite,
    /// Restricted to these rectangles, in surface-local coordinates.
    Rects(Vec<RegionRect>),
}

#[derive(Debug, Default)]
pub struct Region {
    pub client_id: u32,
    /// Add/subtract rectangles in the order the client issued them.
    pub rects: Vec<RegionRect>,
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
    /// Smallest size the client says it can work at, from `set_min_size`.
    /// Zero in a dimension means it named no limit there.
    pub min_size: (i32, i32),
    /// Largest size the client says it wants, from `set_max_size`. Zero in a
    /// dimension means it named no limit there.
    pub max_size: (i32, i32),
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

#[derive(Debug, Default, Clone, Copy, FromRepr)]
#[repr(u32)]
pub enum XdgPositionerAnchor {
    #[default]
    None = 0,
    Top = 1,
    Bottom = 2,
    Left = 3,
    Right = 4,
    TopLeft = 5,
    BottomLeft = 6,
    TopRight = 7,
    BottomRight = 8,
}

#[derive(Debug, Default, Clone, Copy, FromRepr)]
#[repr(u32)]
pub enum XdgPositionerGravity {
    #[default]
    None = 0,
    Top = 1,
    Bottom = 2,
    Left = 3,
    Right = 4,
    TopLeft = 5,
    BottomLeft = 6,
    TopRight = 7,
    BottomRight = 8,
}

#[derive(Debug, Default, Clone, Copy, FromRepr)]
#[repr(u32)]
pub enum XdgPositionerConstraintAdjustment {
    #[default]
    None = 0,
    SlideX = 1,
    SlideY = 2,
    FlipX = 4,
    FlipY = 8,
    ResizeX = 16,
    ResizeY = 32,
}

#[derive(Debug, Default, Clone)]
pub struct XdgPositionerState {
    pub client_id: u32,
    pub width: i32,
    pub height: i32,
    pub anchor_rect: (i32, i32, i32, i32),
    pub anchor: XdgPositionerAnchor,
    pub gravity: XdgPositionerGravity,
    pub offset: (i32, i32),
    pub constraint_adjustment: XdgPositionerConstraintAdjustment,
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
    /// dma-buf formats the backend can import, from
    /// [`crate::shared::BackendMessage::DmabufSupport`].
    ///
    /// Empty until the backend has answered, and empty for good if it cannot
    /// import at all — which is what `zwp_linux_dmabuf_v1` will be advertised
    /// on. A compositor that offers dma-buf it cannot draw is worse than one
    /// that offers none: the client allocates for it and then has nothing to
    /// fall back to.
    pub dmabuf_formats: Vec<crate::shared::DmabufFormat>,
    /// The workspaces of every output, and the windows on them.
    ///
    /// Outputs own workspaces and workspaces own windows, so this is where a
    /// toplevel's output, its stacking order and whether it is on screen at
    /// all are all read from.
    pub workspaces: Workspaces,
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
    /// The interactive move or resize in progress, if any.
    pub pointer_grab: Option<PointerGrab>,
    /// Mouse buttons currently held, as evdev codes.
    pub pressed_buttons: HashSet<u32>,
    /// Serial of each client's most recent `wl_pointer.button` press, which a
    /// client must quote when it asks to start a move or resize.
    pub last_button_serial: HashMap<u32, u32>,
    /// Source of `ShmBuffer::content_serial`. Only ever incremented, so no two
    /// buffer contents in the lifetime of the compositor share a serial.
    pub next_content_serial: u64,
    /// The compositor's own handle on each live buffer's memory. Anything
    /// reading a buffer holds a clone, so a count of one means nobody is.
    pub buffer_guards: HashMap<ClientObjectId, Arc<BufferGuard>>,
    /// Buffers the compositor has finished with, waiting on their readers
    /// before `wl_buffer.release` can be sent.
    pub releasing_buffers: HashSet<ClientObjectId>,
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
            dmabuf_formats: Vec::new(),
            workspaces: Workspaces::new(),
            dirty: true,
            cursor_surfaces: HashMap::new(),
            pointer_enter_serial: HashMap::new(),
            cursor_role_surfaces: HashSet::new(),
            default_cursor: None,
            grabbed_popups: Vec::new(),
            pointer_grab: None,
            pressed_buttons: HashSet::new(),
            last_button_serial: HashMap::new(),
            next_content_serial: 0,
            buffer_guards: HashMap::new(),
            releasing_buffers: HashSet::new(),
        }
    }

    /// Returns false if the pool could not be mapped, which is a client error:
    /// the file is unreadable or smaller than the pool it declared.
    pub fn register_shm_pool(
        &mut self,
        client_id: u32,
        pool_id: u32,
        fd: RawFd,
        size: u32,
    ) -> bool {
        // Reusing a live object id is a protocol error, rejected in
        // `wl_shm::handle_create_pool` before we get here. Guard anyway: a plain
        // insert would drop the displaced pool without unmapping or closing it.
        if let Some(old) = self.shm_pools.remove(&(client_id, pool_id)) {
            tracing::warn!(
                "wl_shm pool {} re-registered for client {}, freeing the displaced mapping",
                pool_id,
                client_id,
            );
            // Dropping the pool drops its `Arc`; the mapping itself survives
            // until whatever is still reading it lets go.
            drop(old.mapping);
            unsafe { libc::close(old.fd) };
        }

        let mapping = PoolMapping::new(fd, size).map(Arc::new);
        let mapped = mapping.is_some();
        self.shm_pools.insert(
            (client_id, pool_id),
            ShmPool {
                client_id,
                fd,
                size,
                mapping,
                dead: false,
            },
        );
        mapped
    }

    pub fn destroy_shm_pool(&mut self, client_id: u32, pool_id: u32) {
        if let Some(pool) = self.shm_pools.get_mut(&(client_id, pool_id)) {
            pool.dead = true;
        }
        self.try_cleanup_pool(client_id, pool_id);
    }

    /// Returns false if the resized pool could not be mapped — see
    /// [`Self::register_shm_pool`].
    pub fn resize_shm_pool(&mut self, client_id: u32, pool_id: u32, new_size: u32) -> bool {
        self.mark_pool_damaged(client_id, pool_id);
        let Some(pool) = self.shm_pools.get_mut(&(client_id, pool_id)) else {
            return false;
        };
        pool.size = new_size;
        // The old mapping is replaced, not unmapped: a frame already in flight
        // may still be reading through it, and it will be unmapped when that
        // reference goes.
        pool.mapping = PoolMapping::new(pool.fd, new_size).map(Arc::new);
        pool.mapping.is_some()
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
        let content_serial = self.next_content_serial();
        // A re-registered id is a fresh buffer: forget any release still owed
        // on the old one, which the client has replaced rather than waited for.
        self.releasing_buffers.remove(&(client_id, buffer_id));
        if let Some(mapping) = self
            .shm_pools
            .get(&(client_id, pool_id))
            .and_then(|pool| pool.mapping.clone())
        {
            self.buffer_guards
                .insert((client_id, buffer_id), Arc::new(BufferGuard::new(mapping)));
        }
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
                content_serial,
                // A buffer nobody has read yet is wholly new.
                damage: None,
            },
        );
    }

    pub fn destroy_buffer(&mut self, client_id: u32, buffer_id: u32) {
        let pool_id = self
            .shm_buffers
            .get(&(client_id, buffer_id))
            .map(|b| b.pool_id);
        self.shm_buffers.remove(&(client_id, buffer_id));
        // The client has destroyed the buffer, so it is not waiting to hear
        // that it may reuse it.
        self.buffer_guards.remove(&(client_id, buffer_id));
        self.releasing_buffers.remove(&(client_id, buffer_id));
        if let Some(pool_id) = pool_id {
            self.try_cleanup_pool(client_id, pool_id);
        }
    }

    /// Begin an interactive move of a window.
    ///
    /// Sends the client a pointer leave first: the compositor owns the pointer for
    /// the duration, and a client left believing the pointer is still inside it
    /// would keep drawing hover states it can no longer see the end of.
    pub fn start_move_grab(&mut self, surface: ClientObjectId) {
        let Some(toplevel) = self.toplevel_for_surface(surface) else {
            return;
        };
        let Some(position) = self.surfaces.get(&surface).map(|s| s.position) else {
            return;
        };
        self.release_pointer_to_grab();
        self.pointer_grab = Some(PointerGrab {
            surface,
            toplevel,
            kind: GrabKind::Move {
                offset_x: position.0 - f64_to_i32(self.cursor_x),
                offset_y: position.1 - f64_to_i32(self.cursor_y),
            },
        });
    }

    /// Begin an interactive resize of a window's edge or corner.
    pub fn start_resize_grab(&mut self, surface: ClientObjectId, edges: ResizeEdges) {
        let Some(toplevel) = self.toplevel_for_surface(surface) else {
            return;
        };
        let Some(position) = self.surfaces.get(&surface).map(|s| s.position) else {
            return;
        };
        let size = self.surface_size(surface);
        if size.0 <= 0 || size.1 <= 0 {
            return;
        }
        self.release_pointer_to_grab();
        self.pointer_grab = Some(PointerGrab {
            surface,
            toplevel,
            kind: GrabKind::Resize {
                edges,
                start_pointer: (self.cursor_x, self.cursor_y),
                start_position: position,
                start_size: size,
                last_sent: size,
            },
        });
    }

    /// Take the pointer away from whichever client currently has it.
    fn release_pointer_to_grab(&mut self) {
        let Some(pointer_surface) = self.pointer_surface.take() else {
            return;
        };
        for ptr in self.pointers.clone() {
            if ptr.client_id == pointer_surface.0 {
                wl_pointer::send_leave(self, ptr.client_id, ptr.object_id, pointer_surface.1);
                wl_pointer::send_frame(self, ptr.client_id, ptr.object_id);
            }
        }
    }

    /// The size range a client has said it will accept.
    ///
    /// Returns `((min_w, min_h), (max_w, max_h))`, where a zero means the
    /// client named no limit in that dimension.
    pub fn client_size_limits(&self, surface: ClientObjectId) -> ((i32, i32), (i32, i32)) {
        self.toplevel_for_surface(surface)
            .and_then(|key| self.xdg_toplevels.get(&key))
            .map_or(((0, 0), (0, 0)), |t| (t.min_size, t.max_size))
    }

    /// The `xdg_toplevel` object driving a window, given its `wl_surface`.
    pub fn toplevel_for_surface(&self, key: ClientObjectId) -> Option<ClientObjectId> {
        self.xdg_surfaces
            .iter()
            .find(|((client_id, _), xdg)| *client_id == key.0 && xdg.wl_surface_id == key.1)
            .and_then(|(&(client_id, _), xdg)| match xdg.role {
                Some(XdgRole::Toplevel(id)) => Some((client_id, id)),
                _ => None,
            })
    }

    /// Put the pointer at a position, or as near to it as the outputs allow.
    pub fn move_cursor_to(&mut self, x: f64, y: f64) {
        let (x, y) = self.constrain_to_outputs(x, y);
        self.cursor_x = x;
        self.cursor_y = y;
    }

    /// Move the pointer by a delta.
    ///
    /// What a mouse actually reports: at the libinput layer a mouse has no
    /// position at all, only movement, and the pointer position is the
    /// compositor's to own and to keep somewhere sensible.
    pub fn move_cursor_by(&mut self, dx: f64, dy: f64) {
        self.move_cursor_to(self.cursor_x + dx, self.cursor_y + dy);
    }

    /// Pull a position onto the outputs, if it is not on one already.
    ///
    /// Constraining matters more than it looks. A relative device gives deltas
    /// and nothing else, so an unconstrained pointer walks off the desktop and
    /// never comes back — and the failure is silent rather than loud: the
    /// cursor stops being drawn, hit testing finds nothing, and clicks go
    /// nowhere, with no edge for the user to push against.
    ///
    /// The constraint is the union of the outputs, not their bounding box.
    /// Two outputs of different heights side by side leave a notch belonging to
    /// no output, and a pointer there would be exactly as lost.
    fn constrain_to_outputs(&self, x: f64, y: f64) -> (f64, f64) {
        // Every reader converts this to `i32` unchecked, so a non-finite value
        // must never be stored.
        if !x.is_finite() || !y.is_finite() {
            return (self.cursor_x, self.cursor_y);
        }
        if self.outputs.is_empty() {
            // Nothing to constrain against, but the conversion still has to be
            // safe.
            return (
                x.clamp(f64::from(i32::MIN), f64::from(i32::MAX)),
                y.clamp(f64::from(i32::MIN), f64::from(i32::MAX)),
            );
        }

        let mut nearest: Option<(f64, f64, f64)> = None;
        for output in &self.outputs {
            let Some((x0, y0, x1, y1)) = cursor_bounds(output) else {
                continue;
            };
            if x >= x0 && x <= x1 && y >= y0 && y <= y1 {
                return (x, y);
            }
            // The closest this output can get to where the pointer wanted to be.
            let (cx, cy) = (x.clamp(x0, x1), y.clamp(y0, y1));
            let distance = (cx - x).powi(2) + (cy - y).powi(2);
            if nearest.is_none_or(|(_, _, best)| distance < best) {
                nearest = Some((cx, cy, distance));
            }
        }
        nearest.map_or((self.cursor_x, self.cursor_y), |(x, y, _)| (x, y))
    }

    /// The output a new window should open on: the one under the pointer, or
    /// failing that whichever output comes first.
    pub fn output_for_new_window(&self) -> Option<OutputId> {
        let (x, y) = (f64_to_i32(self.cursor_x), f64_to_i32(self.cursor_y));
        self.outputs
            .iter()
            .find(|o| output_contains(o, x, y))
            .or_else(|| self.outputs.first())
            .map(|o| o.id)
    }

    /// Size of a window in surface-local coordinates.
    ///
    /// Zero until the client has attached a buffer, which is the usual state
    /// when a toplevel is first mapped.
    pub fn surface_size(&self, key: ClientObjectId) -> (i32, i32) {
        self.surface_buffer_mapping(key)
            .map_or((0, 0), |m| (m.dest_width, m.dest_height))
    }

    /// Give every output the one workspace it starts with, and take back the
    /// windows of any output that has gone. Returns true if anything changed.
    ///
    /// Called when outputs are added or removed, and again every tick, so no
    /// path can leave an output without somewhere to put a window.
    pub fn sync_workspaces(&mut self) -> bool {
        self.workspaces.sync_outputs(&self.outputs)
    }

    /// Which output a window is on: the one owning the workspace it is in.
    ///
    /// `None` for a window waiting to be placed, and for anything that is not
    /// a toplevel — popups and subsurfaces hang off a toplevel and take its
    /// output along with its position.
    pub fn surface_output(&self, surface_key: ClientObjectId) -> Option<OutputId> {
        self.workspaces.output_of(surface_key)
    }

    /// The topmost window the user can currently see and click on: the top of
    /// the workspace showing on the output under the pointer, or failing that
    /// of the first output that has a window.
    pub fn top_visible_toplevel(&self) -> Option<ClientObjectId> {
        self.output_for_new_window()
            .and_then(|output_id| self.workspaces.active(output_id))
            .and_then(Workspace::top)
            .or_else(|| {
                self.outputs
                    .iter()
                    .filter_map(|o| self.workspaces.active(o.id))
                    .find_map(Workspace::top)
            })
    }

    /// Put a newly mapped window on the workspace showing on an output, at
    /// that workspace's next cascade position.
    ///
    /// The window has no buffer yet, so its size is unknown here; the cascade
    /// only has to stay clear of the output's far edges, and
    /// [`Self::confine_toplevels`] pulls the window fully on-screen once its
    /// size is known.
    ///
    /// Returns false if the output has no workspace to put it on, which means
    /// the compositor has not seen that output.
    fn place_toplevel(&mut self, surface_key: ClientObjectId, output_id: OutputId) -> bool {
        let Some(output) = self.outputs.iter().find(|o| o.id == output_id) else {
            return false;
        };
        let (origin_x, origin_y) = (output.geometry.x, output.geometry.y);
        let (limit_x, limit_y) = (
            output.geometry.physical_width / 2,
            output.geometry.physical_height / 2,
        );

        let Some(workspace) = self.workspaces.active_mut(output_id) else {
            return false;
        };
        let (local_x, local_y) = workspace.next_cascade_slot(limit_x, limit_y);
        workspace.raise(surface_key);

        if let Some(surface) = self.surfaces.get_mut(&surface_key) {
            surface.position = (origin_x + local_x, origin_y + local_y);
        }
        true
    }

    /// Move a window to the workspace showing on another output, taking it off
    /// the one it was on. Returns true if it ended up somewhere new.
    pub fn move_toplevel_to_output(
        &mut self,
        surface_key: ClientObjectId,
        output_id: OutputId,
    ) -> bool {
        if self.workspaces.output_of(surface_key) == Some(output_id) {
            return false;
        }
        self.workspaces.place(output_id, surface_key)
    }

    /// Keep every window on an output that exists, and wholly inside it.
    ///
    /// A window belongs to one output and may not straddle two, so this both
    /// re-homes windows whose output has gone (or that were mapped before any
    /// output existed) and clamps positions back inside after a window is
    /// resized by its client or an output changes size.
    ///
    /// Returns true if anything moved.
    pub fn confine_toplevels(&mut self) -> bool {
        // Outputs come and go without the workspaces hearing about it, so they
        // are reconciled here too: a new output gets its workspace, and the
        // windows of one that has gone come back as unplaced.
        let mut moved = self.sync_workspaces();

        for key in self.workspaces.take_unplaced() {
            let placed = self
                .output_for_new_window()
                .is_some_and(|output_id| self.place_toplevel(key, output_id));
            if placed {
                moved = true;
            } else {
                // Still nowhere to put it: hold it until an output turns up.
                self.workspaces.hold_unplaced(key);
            }
        }

        for (output_id, key) in self.workspaces.windows().collect::<Vec<_>>() {
            let (width, height) = self.surface_size(key);
            let Some(output) = self.outputs.iter().find(|o| o.id == output_id) else {
                continue;
            };
            // Copied out so the surface can be borrowed mutably below.
            let (ox, oy, ow, oh) = (
                output.geometry.x,
                output.geometry.y,
                output.geometry.physical_width,
                output.geometry.physical_height,
            );
            let Some(surface) = self.surfaces.get_mut(&key) else {
                continue;
            };

            // A window too big for its output is pinned to the top-left; there
            // is no position that fits, and the top-left is the useful part.
            let max_x = (ox + ow - width).max(ox);
            let max_y = (oy + oh - height).max(oy);
            let confined = (
                surface.position.0.clamp(ox, max_x),
                surface.position.1.clamp(oy, max_y),
            );
            if surface.position != confined {
                surface.position = confined;
                moved = true;
            }
        }
        moved
    }

    /// Work out which part of a surface's buffer is shown, and at what size.
    ///
    /// A `wp_viewport` source crops and a viewport destination sizes; failing
    /// either, the whole buffer is shown divided down by the surface's buffer
    /// scale, which is what turns an oversized buffer from a scaled client back
    /// into its logical size on screen.
    ///
    /// Shared by scene building and by damage mapping, which needs to run this
    /// backwards — the two must agree or damage lands in the wrong place.
    pub fn surface_buffer_mapping(&self, key: ClientObjectId) -> Option<BufferMapping> {
        let surface = self.surfaces.get(&key)?;
        let buffer = self.shm_buffers.get(&(key.0, surface.buffer_id?))?;
        if buffer.width <= 0 || buffer.height <= 0 {
            return None;
        }

        let viewport = self
            .surface_viewport
            .get(&key)
            .and_then(|&vp_id| self.viewports.get(&(key.0, vp_id)));

        let src = viewport.and_then(|v| v.source).unwrap_or((
            0.0,
            0.0,
            f64::from(buffer.width),
            f64::from(buffer.height),
        ));
        let scale = surface.buffer_scale.max(1);
        let (dest_width, dest_height) = match viewport.and_then(|v| v.destination) {
            Some((w, h)) => (w, h),
            None => (f64_to_i32(src.2) / scale, f64_to_i32(src.3) / scale),
        };
        if dest_width <= 0 || dest_height <= 0 {
            return None;
        }

        Some(BufferMapping {
            src,
            dest_width,
            dest_height,
        })
    }

    /// Hand out the next content serial.
    fn next_content_serial(&mut self) -> u64 {
        self.next_content_serial += 1;
        self.next_content_serial
    }

    /// Record that a buffer's contents have changed, and how much of it.
    ///
    /// An empty `damage` means the whole buffer must be treated as new. Damage
    /// accumulates across changes nobody has read yet: if two commits land
    /// between two scenes, the second must not erase what the first reported.
    pub fn mark_buffer_damaged(&mut self, client_id: u32, buffer_id: u32, damage: &[TextureRect]) {
        let serial = self.next_content_serial();
        if let Some(buffer) = self.shm_buffers.get_mut(&(client_id, buffer_id)) {
            buffer.content_serial = serial;
            match (&mut buffer.damage, damage.is_empty()) {
                // Widening to "everything" is sticky until the damage is read:
                // once one change could not be described, no later rectangle
                // can narrow the window back down.
                (_, true) | (None, false) => buffer.damage = None,
                (Some(existing), false) => existing.extend_from_slice(damage),
            }
        }
    }

    /// Whether anything is still reading a buffer's memory.
    ///
    /// True only while some `TextureImage` borrowing it is alive. A buffer with
    /// no guard at all — destroyed, or never mapped — counts as idle.
    pub fn buffer_is_being_read(&self, key: ClientObjectId) -> bool {
        self.buffer_guards
            .get(&key)
            .is_some_and(|guard| Arc::strong_count(guard) > 1)
    }

    /// Forget the damage accumulated so far, having acted on it.
    ///
    /// Resets to "nothing has changed" rather than to "unknown" — the two are
    /// opposites, and clearing to the wrong one would either force a full
    /// upload every frame or skip a real change.
    pub fn clear_buffer_damage(&mut self) {
        for buffer in self.shm_buffers.values_mut() {
            buffer.damage = Some(Vec::new());
        }
    }

    /// Mark every buffer backed by a pool as wholly changed.
    ///
    /// Used when the mapping itself moves, which invalidates any copy taken
    /// from it regardless of what the client did or did not draw.
    fn mark_pool_damaged(&mut self, client_id: u32, pool_id: u32) {
        let keys: Vec<ClientObjectId> = self
            .shm_buffers
            .iter()
            .filter(|((cid, _), b)| *cid == client_id && b.pool_id == pool_id)
            .map(|(&key, _)| key)
            .collect();
        for (cid, bid) in keys {
            self.mark_buffer_damaged(cid, bid, &[]);
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
            drop(pool.mapping);
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
                input_region: None,
                buffer_scale: 1,
                entered_outputs: HashSet::new(),
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
                min_size: (0, 0),
                max_size: (0, 0),
            },
        );
        let Some(xdg_surface) = self.xdg_surfaces.get_mut(&(client_id, xdg_surface_id)) else {
            return;
        };
        xdg_surface.role = Some(XdgRole::Toplevel(toplevel_id));
        let surface_key = (client_id, xdg_surface.wl_surface_id);

        // A window opens on top of the workspace showing on its output. If
        // there is no output yet it is held aside, and `confine_toplevels`
        // places it once one appears.
        let placed = self
            .output_for_new_window()
            .is_some_and(|output_id| self.place_toplevel(surface_key, output_id));
        if !placed {
            self.workspaces.hold_unplaced(surface_key);
        }
        self.dirty = true;
    }

    pub fn destroy_xdg_toplevel(&mut self, client_id: u32, toplevel_id: u32) {
        if let Some(toplevel) = self.xdg_toplevels.remove(&(client_id, toplevel_id)) {
            // Remove the associated wl_surface from the stack and clear focus
            if let Some(xdg_surface) = self.xdg_surfaces.get(&(client_id, toplevel.xdg_surface_id))
            {
                let surface_key = (client_id, xdg_surface.wl_surface_id);
                self.workspaces.remove(surface_key);
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
        self.buffer_guards.retain(|&(cid, _), _| cid != client_id);
        self.releasing_buffers.retain(|&(cid, _)| cid != client_id);
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
        self.last_button_serial.remove(&client_id);
        if self.pointer_grab.is_some_and(|g| g.surface.0 == client_id) {
            self.pointer_grab = None;
        }
        self.workspaces.remove_client(client_id);
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

#[cfg(test)]
mod tests;
