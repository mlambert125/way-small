//! GPU buffers shared as dma-buf file descriptors.
//!
//! A client that renders on the GPU has its pixels there already. Handing them
//! over as an shm pool means reading them back to the CPU, copying them across
//! a socket-shared mapping, and uploading them again — three trips for pixels
//! that never needed to leave the card. A dma-buf is the kernel's handle on
//! that memory instead: the client passes a file descriptor, the compositor
//! imports it as a texture, and nothing is copied at all.
//!
//! These types are the description that travels with those descriptors. They
//! carry no GL or EGL of their own: the compositor never imports anything,
//! because importing needs the GL context that only a backend has. What the
//! compositor does is decide which buffer belongs on screen and hand the
//! description to the backend, which is the same division of labour the shm
//! path already uses.
//!
//! Nothing here is Wayland-shaped. `zwp_linux_dmabuf_v1` is how a client will
//! eventually describe one of these, but the description is independent of the
//! protocol that carried it, and the backend's import path should not have to
//! know a client was involved at all.

use std::os::fd::OwnedFd;
use std::sync::Arc;

/// `DRM_FORMAT_ARGB8888`: 32-bit BGRA, premultiplied alpha, little-endian.
///
/// A DRM fourcc, which is not the same numbering as `wl_shm`'s despite
/// describing the same bytes — `wl_shm` gives ARGB8888 and XRGB8888 the
/// special values 0 and 1, and every other format the fourcc.
pub const DRM_FORMAT_ARGB8888: u32 = fourcc(*b"AR24");
/// `DRM_FORMAT_XRGB8888`: as [`DRM_FORMAT_ARGB8888`], with the alpha byte
/// undefined and the image treated as opaque.
pub const DRM_FORMAT_XRGB8888: u32 = fourcc(*b"XR24");

/// The modifier meaning "unspecified".
///
/// Not a layout at all but the absence of a claim about one: the buffer's real
/// layout is whatever the two drivers agreed out of band. Importing one is
/// asking the driver to guess, which it can only do when the same hardware
/// allocated it — which is why a modifier-aware client sends a real modifier
/// and this exists for the ones that predate them.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Build a fourcc from its four characters, as the DRM headers do.
///
/// The inverse of [`fourcc_name`], and the readable way to name a format the
/// compositor has no constant for: `fourcc(*b"NV12")`.
pub const fn fourcc(code: [u8; 4]) -> u32 {
    (code[0] as u32) | ((code[1] as u32) << 8) | ((code[2] as u32) << 16) | ((code[3] as u32) << 24)
}

/// Render a fourcc back into the four characters it was built from, for logs.
///
/// Anything unprintable comes back as `?`, so a garbage format still produces
/// a line that can be read rather than one that mangles the terminal.
pub fn fourcc_name(code: u32) -> String {
    code.to_le_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() {
                char::from(b)
            } else {
                '?'
            }
        })
        .collect()
}

/// One plane of a dma-buf image.
///
/// Most formats have exactly one. Multi-planar ones — the YUV layouts a video
/// decoder produces — split the image across several, which may or may not be
/// separate descriptors: two planes commonly share one buffer at different
/// offsets.
#[derive(Debug)]
pub struct DmabufPlane {
    /// The descriptor this plane's memory lives in.
    ///
    /// Shared rather than owned because the same descriptor can appear in
    /// several planes, and because the backend holds the image for as long as
    /// it is drawing from it while the compositor may already have moved on.
    pub fd: Arc<OwnedFd>,
    /// Byte offset of the plane within the descriptor.
    pub offset: u32,
    /// Distance between rows, in bytes.
    pub stride: u32,
}

/// A GPU buffer, described well enough to import.
///
/// The compositor treats this as opaque: it knows the size, so it can lay the
/// buffer out in a scene, and nothing else. Whether the description is one a
/// driver will actually accept is a question only the backend can answer, and
/// it answers it by trying.
#[derive(Debug)]
pub struct DmabufImage {
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// DRM fourcc describing the pixel layout.
    pub fourcc: u32,
    /// DRM format modifier: the tiling and compression the pixels are stored
    /// with. [`DRM_FORMAT_MOD_INVALID`] means the buffer carries no claim.
    pub modifier: u64,
    /// The planes, in the order the format defines them.
    pub planes: Vec<DmabufPlane>,
}

/// A format a backend can import, and the modifiers it accepts for it.
///
/// This is what will eventually be advertised to clients, so that a client
/// allocates something importable in the first place rather than finding out
/// after the fact. It comes from the driver, which means it comes from the
/// backend thread, which is why it travels as a message rather than being
/// something the compositor could work out for itself.
#[derive(Debug, Clone)]
pub struct DmabufFormat {
    /// DRM fourcc.
    pub fourcc: u32,
    /// Modifiers accepted for it. Empty means the driver named none, and only
    /// [`DRM_FORMAT_MOD_INVALID`] will import.
    pub modifiers: Vec<u64>,
}

/// What happened when a backend tried its import path end to end.
///
/// The point of asking is that "the extensions are present" and "importing
/// works" are different claims, and only the second one matters. A driver can
/// advertise the entry points and still refuse every buffer.
#[derive(Debug, Clone)]
pub enum DmabufProbe {
    /// A dma-buf was imported and read back with the pixels it went in with.
    Passed,
    /// There is no import path here at all — a backend with no GPU, or a
    /// driver missing the extensions. Not an error: it is the answer for the
    /// headless backend, and the compositor's cue not to advertise dma-buf.
    Unsupported(String),
    /// The import path is there but could not be exercised — the backend has
    /// no way to make a dma-buf of its own to try. Not a failure and not a
    /// pass: the first client to hand one over is what will settle it.
    Untested(String),
    /// The path exists and did not work, which is a real fault worth reporting.
    Failed(String),
}
