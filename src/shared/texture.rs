//! Textures crossing from the compositor to a backend.
//!
//! A texture is a reference to a client's pixels plus everything a backend
//! needs to upload them: what changed since last time, and what the backend is
//! already holding.

use super::buffer::BufferGuard;
use std::sync::Arc;

/// Identity of a texture, used by the backend to cache GPU textures across
/// frames. Stable for as long as the underlying resource lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextureId {
    /// A client `wl_buffer`, keyed by (`client_id`, `buffer_id`).
    Buffer(u32, u32),
    /// The cursor loaded from the system cursor theme.
    DefaultCursor,
    /// The built-in cursor used when no theme is available.
    FallbackCursor,
}

/// Pixel layout of a texture's source bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// `WL_SHM_FORMAT_ARGB8888` — premultiplied alpha.
    Argb8888,
    /// `WL_SHM_FORMAT_XRGB8888` — the high byte is undefined, treat as opaque.
    Xrgb8888,
}

/// An axis-aligned rectangle in texture pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureRect {
    /// Location X
    pub x: i32,
    /// Location Y
    pub y: i32,
    /// Width of the texture
    pub width: i32,
    /// Height of the texture
    pub height: i32,
}

/// Where a texture's pixels live.
#[derive(Debug)]
pub enum TexturePixels {
    /// Borrowed straight from the client's shm mapping — no copy taken. The
    /// guard keeps the mapping alive and holds off `wl_buffer.release` until
    /// every reader is done.
    Mapped {
        /// Guard keeping the mapping alive
        guard: Arc<BufferGuard>,
        /// Byte offset of the first pixel within the mapping.
        offset: usize,
        /// Distance between rows, in bytes. Not necessarily `width * 4`.
        stride: usize,
    },
    /// The compositor's own pixels, tightly packed at `width * 4`. Used for
    /// cursors, which have no client buffer behind them, and for the rare
    /// client layout that GL cannot read in place.
    Owned(Box<[u8]>),
}

/// Pixel data waiting to be uploaded to the GPU.
///
/// Bytes are little-endian `0xAARRGGBB`, i.e. `[B, G, R, A]` per pixel.
///
/// `serial` changes whenever the contents change under a stable `id`, which is
/// how the backend knows a cached texture needs re-uploading. Once dmabuf lands
/// this gains a variant holding an imported GPU buffer, and the upload path
/// disappears for clients that can use it.
#[derive(Debug)]
pub struct TextureImage {
    /// Id
    pub id: TextureId,
    /// A serial number for this texture
    pub serial: u64,
    /// The serial this image was derived from, if it was derived from one.
    ///
    /// `damage` is only meaningful relative to this. A backend holding a
    /// texture at exactly this serial can patch it; anything else — a texture
    /// it never had, or one several serials behind — has to take the whole
    /// image. This is what keeps partial uploads safe even though the backend
    /// evicts textures without telling anyone.
    pub previous_serial: Option<u64>,
    /// Width of the image
    pub width: i32,
    /// Height of the image
    pub height: i32,
    /// Pixel format
    pub format: PixelFormat,
    /// The complete current contents. Always addressable in full, even when
    /// `damage` is not, so a backend that cannot use the damage can fall back.
    pub pixels: TexturePixels,
    /// What changed since `previous_serial`. Empty means "all of it".
    pub damage: Vec<TextureRect>,
}

impl TextureImage {
    /// Row stride in pixels, for GL's `UNPACK_ROW_LENGTH`.
    pub fn row_length(&self) -> i32 {
        match &self.pixels {
            TexturePixels::Mapped { stride, .. } => i32::try_from(stride / 4).unwrap_or(self.width),
            TexturePixels::Owned(_) => self.width,
        }
    }

    /// Borrow the pixels, from the first byte of the image to the last.
    ///
    /// # Safety
    /// For a mapped image this borrows a client's shm mapping, which the client
    /// may write to whenever it is allowed to. What makes reading it sound is
    /// the protocol: a client must not touch a committed buffer until it is
    /// released, and the compositor holds `wl_buffer.release` back until every
    /// `TextureImage` borrowing it has been dropped. The caller must therefore
    /// not keep the slice past the life of this image.
    ///
    /// Returns `None` if the image does not fit its mapping, which should be
    /// impossible — the extent is checked when the image is built — but is
    /// worth failing softly rather than reading out of bounds.
    pub unsafe fn bytes(&self) -> Option<&[u8]> {
        match &self.pixels {
            TexturePixels::Owned(bytes) => Some(bytes),
            TexturePixels::Mapped {
                guard,
                offset,
                stride,
            } => {
                let rows = self.height.unsigned_abs() as usize;
                let width_bytes = self.width.unsigned_abs() as usize * 4;
                // The last row needs no stride padding, so the extent is
                // shorter than `rows * stride`.
                let len = rows.checked_sub(1)?.checked_mul(*stride)? + width_bytes;
                // SAFETY: delegated to this function's own contract.
                unsafe { guard.mapping().slice(*offset, len) }
            }
        }
    }
}
