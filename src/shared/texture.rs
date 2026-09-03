//! Textures crossing from the compositor to a backend.
//!
//! A texture is a reference to a client's pixels plus everything a backend
//! needs to get at them. For most that means an upload, and the description
//! carries what changed since last time and what the backend is already
//! holding; for a client's own GPU buffer it means an import, and there is
//! nothing to send at all. Which of the two it is, is [`TextureSource`].

use super::buffer::BufferGuard;
use super::dmabuf::DmabufImage;
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
    /// A single translucent white pixel, stretched over an output for the
    /// visual bell.
    BellFlash,
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

/// Where the bytes of an uploaded texture live.
///
/// Both kinds go up the same way — `tex_image_2d` from a slice — and differ
/// only in who owns the memory underneath.
#[derive(Debug)]
pub enum UploadPixels {
    /// Shared Memory Pixels (zerocopy through guard)
    Mapped {
        /// Guard keeping the mapping alive
        guard: Arc<BufferGuard>,
        /// Byte offset of the first pixel within the mapping.
        offset: usize,
        /// Distance between rows, in bytes. Not necessarily `width * 4`.
        stride: usize,
    },
    /// Compositor created pixels.  (Cursors, etc.)
    Owned(Box<[u8]>),
}

/// Where a texture comes from, and what the backend has to do to get it.
///
/// The two arms differ in more than storage: an upload can be patched from the
/// copy the backend already holds, which is what `previous_serial` and
/// `damage` describe. An imported buffer has no such notion — the client draws
/// into memory the texture already samples, and nothing crosses this boundary
/// when it does. Those fields therefore live here rather than on
/// [`TextureImage`], where they would have to be given a meaningless value for
/// half the images that exist.
#[derive(Debug)]
pub enum TextureSource {
    /// Bytes the backend has to send to the GPU.
    Upload {
        /// The bytes themselves.
        pixels: UploadPixels,
        /// The serial this image was derived from, if it was derived from one.
        ///
        /// `damage` is only meaningful relative to this. A backend holding a
        /// texture at exactly this serial can patch it; anything else — a
        /// texture it never had, or one several serials behind — has to take
        /// the whole image. This is what keeps partial uploads safe even
        /// though the backend evicts textures without telling anyone.
        previous_serial: Option<u64>,
        /// What changed since `previous_serial`. Empty means "all of it".
        damage: Vec<TextureRect>,
    },
    /// Already on the GPU, shared as dma-buf descriptors. There are no bytes
    /// to send: the backend imports the descriptors as a texture and samples
    /// the client's own memory.
    ///
    /// Shared rather than owned so that the same buffer redrawn across frames
    /// is the same image to the backend, which is how it knows the import it
    /// already has is still good.
    ///
    /// Nothing builds one yet: the backend can import them and the renderer can
    /// draw them, but a client has no way to send one until
    /// `zwp_linux_dmabuf_v1` is implemented.
    #[allow(dead_code)]
    Dmabuf(Arc<DmabufImage>),
}

/// One texture to draw, and everything the backend needs to get hold of it.
///
/// The fields here are the ones every kind of texture has, and they are what
/// the drawing path reads: what to cache it under, how big it is, and whether
/// its alpha means anything. Where the pixels actually come from — and what,
/// if anything, has to be sent to the GPU — is [`TextureSource`], because that
/// is the only part that differs.
///
/// Bytes, where there are any, are little-endian `0xAARRGGBB`, i.e.
/// `[B, G, R, A]` per pixel.
///
/// `serial` changes whenever the contents change under a stable `id`, which is
/// how the backend knows a cached texture needs re-uploading. For a dma-buf it
/// says only that the description changed, not the pixels: a client drawing
/// into a buffer the GPU already holds changes what is sampled without
/// anything crossing this boundary.
#[derive(Debug)]
pub struct TextureImage {
    /// Id
    pub id: TextureId,
    /// A serial number for this texture
    pub serial: u64,
    /// Width of the image
    pub width: i32,
    /// Height of the image
    pub height: i32,
    /// Pixel format
    pub format: PixelFormat,
    /// Where the pixels come from. Always addressable in full, even when the
    /// damage on an upload is not, so a backend that cannot use the damage can
    /// fall back.
    pub source: TextureSource,
}

impl TextureImage {
    /// Row stride in pixels, for GL's `UNPACK_ROW_LENGTH`.
    pub fn row_length(&self) -> i32 {
        match &self.source {
            TextureSource::Upload {
                pixels: UploadPixels::Mapped { stride, .. },
                ..
            } => i32::try_from(stride / 4).unwrap_or(self.width),
            _ => self.width,
        }
    }

    /// Whether the source bytes are `[B, G, R, A]` and need swizzling when
    /// sampled.
    ///
    /// True for everything that goes up through `tex_image_2d`: GLES has no
    /// guaranteed BGRA upload format, so those bytes are uploaded as RGBA
    /// untouched and put right in the shader. An imported dma-buf is not
    /// uploaded at all — the driver is told the real format and samples it
    /// correctly — so swizzling one would undo what the import got right.
    pub fn swizzle_bgra(&self) -> bool {
        matches!(self.source, TextureSource::Upload { .. })
    }

    /// The GPU buffer behind this image, if that is what it is.
    pub fn dmabuf(&self) -> Option<&Arc<DmabufImage>> {
        match &self.source {
            TextureSource::Dmabuf(image) => Some(image),
            TextureSource::Upload { .. } => None,
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
        // On the GPU already; there is nothing here to read.
        let TextureSource::Upload { pixels, .. } = &self.source else {
            return None;
        };
        match pixels {
            UploadPixels::Owned(bytes) => Some(bytes),
            UploadPixels::Mapped {
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
