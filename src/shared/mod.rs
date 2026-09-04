//! The vocabulary the subsystems share.

pub mod buffer_guard;
pub mod clock;
pub mod dmabuf;
pub mod input;
pub mod message;
pub mod output;
pub mod pool_mapping;
pub mod scene;
mod shm_guard;
pub mod texture;

pub use buffer_guard::BufferGuard;
pub use clock::PresentedAt;
pub use dmabuf::{
    DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888,
    DmabufFormat, DmabufImage, DmabufPlane, DmabufProbe, fourcc_name, pixel_format,
};
pub use input::{ButtonState, KeyState, MouseButton, ScrollSource};
pub use message::{BackendMessage, BackendRequest};
pub use output::{
    OUTPUT_MODE_CURRENT, OUTPUT_MODE_PREFERRED, Output, OutputGeometry, OutputId, OutputMode,
    OutputSubpixel, OutputTransform, cursor_bounds, output_contains,
};
pub use pool_mapping::PoolMapping;
pub use scene::{BACKGROUND_COLOR, BufferTransform, Frame, Scene, SceneElement};
pub use shm_guard::patched_pages;
pub use texture::{PixelFormat, TextureId, TextureImage, TextureRect, TextureSource, UploadPixels};
