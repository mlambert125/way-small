//! dma-buf import, through EGL.
//!
//! Turns the descriptors in a [`DmabufImage`] into an `EGLImage`, which the
//! renderer then binds as an ordinary GL texture. Nothing is copied: the
//! texture samples the memory the client already rendered into.
//!
//! The entry points are all extensions, loaded by name through the same
//! `eglGetProcAddress` the rest of GL comes in on, so this needs no new
//! dependency — only the constants from the extension specs, which are
//! declared below because no crate in the tree carries them. What is used:
//!
//! - `EGL_EXT_image_dma_buf_import` — the import itself. Without it there is
//!   no dma-buf support at all and [`DmabufImporter::new`] refuses.
//! - `EGL_EXT_image_dma_buf_import_modifiers` — enumerating what the driver
//!   will actually take. Without it the format list falls back to the two RGB
//!   formats every driver imports, with no modifier.
//! - `EGL_MESA_image_dma_buf_export` — the reverse direction, used only by
//!   [`DmabufImporter::self_test`] to make a real dma-buf to import.
//!
//! Everything here must run on the thread holding the GL context, like the
//! rest of the renderer.

use std::ffi::{CStr, c_char, c_void};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;

use glow::HasContext;
use tracing::debug;

use crate::shared::{
    DRM_FORMAT_MOD_INVALID, DmabufFormat, DmabufImage, DmabufPlane, DmabufProbe, fourcc_name,
};

// EGL types, as the headers define them.
type EGLDisplay = *const c_void;
type EGLContext = *const c_void;
type EGLClientBuffer = *const c_void;
type EGLImageKHR = *const c_void;
type EGLBoolean = u32;
type EGLenum = u32;
type EGLint = i32;

// Core EGL.
const EGL_NONE: EGLint = 0x3038;
const EGL_TRUE: EGLint = 1;
const EGL_EXTENSIONS: EGLint = 0x3055;
const EGL_HEIGHT: EGLint = 0x3056;
const EGL_WIDTH: EGLint = 0x3057;
const EGL_NO_CONTEXT: EGLContext = std::ptr::null();
const EGL_NO_IMAGE_KHR: EGLImageKHR = std::ptr::null();

// EGL_KHR_image_base.
const EGL_IMAGE_PRESERVED_KHR: EGLint = 0x30D2;
// EGL_KHR_gl_image, for exporting a texture in the self-test.
const EGL_GL_TEXTURE_2D_KHR: EGLenum = 0x30B1;

// EGL_EXT_image_dma_buf_import. Only plane 0 is named here: a multi-planar
// image cannot be sampled with the `sampler2D` this renderer has — YUV needs
// `samplerExternalOES` and a second program — so `import` refuses one, and the
// plane 1..3 attributes would have no caller.
const EGL_LINUX_DMA_BUF_EXT: EGLenum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: EGLint = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EGLint = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: EGLint = 0x3274;
// EGL_EXT_image_dma_buf_import_modifiers. A modifier is 64 bits and an
// attribute value is 32, which is why it arrives in halves.
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EGLint = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EGLint = 0x3444;

/// Side of the image [`DmabufImporter::self_test`] round-trips.
///
/// Small enough to be free, big enough that a stride mistake shows up as a
/// shear rather than passing by luck.
const SELF_TEST_SIDE: i32 = 4;

/// Formats assumed importable when the driver will not enumerate them.
///
/// The base import extension has no way to ask, so this is the floor every
/// driver implementing it meets: the two 8-bit RGB layouts, with no modifier.
const ASSUMED_FORMATS: [u32; 2] = [
    crate::shared::DRM_FORMAT_ARGB8888,
    crate::shared::DRM_FORMAT_XRGB8888,
];

type PfnGetCurrentContext = unsafe extern "system" fn() -> EGLContext;
type PfnQueryString = unsafe extern "system" fn(EGLDisplay, EGLint) -> *const c_char;
type PfnCreateImageKhr = unsafe extern "system" fn(
    EGLDisplay,
    EGLContext,
    EGLenum,
    EGLClientBuffer,
    *const EGLint,
) -> EGLImageKHR;
type PfnDestroyImageKhr = unsafe extern "system" fn(EGLDisplay, EGLImageKHR) -> EGLBoolean;
type PfnImageTargetTexture2DOes = unsafe extern "system" fn(EGLenum, EGLImageKHR);
type PfnQueryDmabufFormats =
    unsafe extern "system" fn(EGLDisplay, EGLint, *mut EGLint, *mut EGLint) -> EGLBoolean;
type PfnQueryDmabufModifiers = unsafe extern "system" fn(
    EGLDisplay,
    EGLint,
    EGLint,
    *mut u64,
    *mut EGLBoolean,
    *mut EGLint,
) -> EGLBoolean;
type PfnExportDmabufQueryMesa = unsafe extern "system" fn(
    EGLDisplay,
    EGLImageKHR,
    *mut EGLint,
    *mut EGLint,
    *mut u64,
) -> EGLBoolean;
type PfnExportDmabufMesa = unsafe extern "system" fn(
    EGLDisplay,
    EGLImageKHR,
    *mut EGLint,
    *mut EGLint,
    *mut EGLint,
) -> EGLBoolean;

/// An `EGLImage`, destroyed when dropped.
///
/// Held alongside the GL texture it was bound to rather than destroyed after
/// binding. The two are siblings referring to the same memory, and keeping the
/// handle is what lets the texture be rebound — and makes the lifetime of the
/// import something the cache can reason about instead of a side effect.
pub struct EglImage {
    /// The image handle.
    raw: EGLImageKHR,
    /// The display it belongs to, needed to destroy it.
    display: EGLDisplay,
    /// `eglDestroyImageKHR`, kept here so dropping needs no other context.
    destroy: PfnDestroyImageKhr,
}

impl Drop for EglImage {
    fn drop(&mut self) {
        // SAFETY: the handle came from `eglCreateImageKHR` on this display and
        // is destroyed exactly once, here.
        unsafe { (self.destroy)(self.display, self.raw) };
    }
}

/// The EGL entry points needed to import a dma-buf, resolved once.
pub struct DmabufImporter {
    /// The display every call is made against.
    display: EGLDisplay,
    /// `eglCreateImageKHR`.
    create_image: PfnCreateImageKhr,
    /// `eglDestroyImageKHR`.
    destroy_image: PfnDestroyImageKhr,
    /// `glEGLImageTargetTexture2DOES`, which is GL rather than EGL but comes
    /// in through the same loader.
    image_target_texture: PfnImageTargetTexture2DOes,
    /// `eglQueryDmaBufFormatsEXT` and `eglQueryDmaBufModifiersEXT`, absent
    /// unless the modifiers extension is there.
    query: Option<(PfnQueryDmabufFormats, PfnQueryDmabufModifiers)>,
    /// `eglExportDMABUFImageQueryMESA` and `eglExportDMABUFImageMESA`, used
    /// only by [`Self::self_test`].
    export: Option<(PfnExportDmabufQueryMesa, PfnExportDmabufMesa)>,
    /// `eglGetCurrentContext`, likewise: exporting a texture needs the context
    /// that owns it.
    current_context: Option<PfnGetCurrentContext>,
}

impl DmabufImporter {
    /// Resolve the entry points, or say why dma-buf import is unavailable.
    ///
    /// # Safety
    /// `display` must be the live `EGLDisplay` the GL context was made on, and
    /// `loader` must resolve symbols for it. The importer must then be used
    /// only while that context is current on this thread.
    pub unsafe fn new(
        display: EGLDisplay,
        loader: &dyn Fn(&CStr) -> *const c_void,
    ) -> Result<Self, String> {
        if display.is_null() {
            return Err("no EGL display: this backend is not running on EGL".into());
        }

        // SAFETY: `eglQueryString` is core EGL and the display is live.
        let extensions = unsafe {
            let query: PfnQueryString = load(loader, c"eglQueryString")
                .ok_or_else(|| String::from("eglQueryString is missing"))?;
            let raw = query(display, EGL_EXTENSIONS);
            if raw.is_null() {
                String::new()
            } else {
                CStr::from_ptr(raw).to_string_lossy().into_owned()
            }
        };

        if !has_extension(&extensions, "EGL_EXT_image_dma_buf_import") {
            return Err("EGL_EXT_image_dma_buf_import is not supported by this driver".into());
        }

        // SAFETY: the extension is present, so these entry points exist.
        let (create_image, destroy_image, image_target_texture) = unsafe {
            (
                load::<PfnCreateImageKhr>(loader, c"eglCreateImageKHR")
                    .ok_or_else(|| String::from("eglCreateImageKHR is missing"))?,
                load::<PfnDestroyImageKhr>(loader, c"eglDestroyImageKHR")
                    .ok_or_else(|| String::from("eglDestroyImageKHR is missing"))?,
                load::<PfnImageTargetTexture2DOes>(loader, c"glEGLImageTargetTexture2DOES")
                    .ok_or_else(|| String::from("glEGLImageTargetTexture2DOES is missing"))?,
            )
        };

        // SAFETY: each is loaded only after its extension is confirmed, and a
        // null return leaves the pair `None` rather than a dangling pointer.
        let query = has_extension(&extensions, "EGL_EXT_image_dma_buf_import_modifiers")
            .then(|| unsafe {
                Some((
                    load::<PfnQueryDmabufFormats>(loader, c"eglQueryDmaBufFormatsEXT")?,
                    load::<PfnQueryDmabufModifiers>(loader, c"eglQueryDmaBufModifiersEXT")?,
                ))
            })
            .flatten();
        let export = has_extension(&extensions, "EGL_MESA_image_dma_buf_export")
            .then(|| unsafe {
                Some((
                    load::<PfnExportDmabufQueryMesa>(loader, c"eglExportDMABUFImageQueryMESA")?,
                    load::<PfnExportDmabufMesa>(loader, c"eglExportDMABUFImageMESA")?,
                ))
            })
            .flatten();
        // SAFETY: core EGL, and optional here — its absence only costs the
        // self-test.
        let current_context =
            unsafe { load::<PfnGetCurrentContext>(loader, c"eglGetCurrentContext") };

        Ok(Self {
            display,
            create_image,
            destroy_image,
            image_target_texture,
            query,
            export,
            current_context,
        })
    }

    /// The formats this driver will import, with their modifiers.
    ///
    /// Modifiers the driver marks `external_only` are dropped: they can only be
    /// sampled through `samplerExternalOES`, which this renderer has no program
    /// for. A format left with none of its modifiers after that filtering goes
    /// too — on a Mesa driver that is every YUV layout, and offering one would
    /// invite a buffer that imports cleanly and then cannot be drawn. They come
    /// back when there is an external-sampler program to draw them with.
    pub fn formats(&self) -> Vec<DmabufFormat> {
        let Some((query_formats, query_modifiers)) = self.query else {
            // No way to ask. Everything implementing the base extension takes
            // these two with an implicit layout.
            return ASSUMED_FORMATS
                .iter()
                .map(|&fourcc| DmabufFormat {
                    fourcc,
                    modifiers: Vec::new(),
                })
                .collect();
        };

        // SAFETY: the entry points came from the extension that defines them,
        // and each call is made with a count matching the buffer handed over —
        // first with none, to learn the count, then with exactly that many.
        let fourccs = unsafe {
            let mut count: EGLint = 0;
            if query_formats(self.display, 0, std::ptr::null_mut(), &raw mut count) != 1
                || count <= 0
            {
                return Vec::new();
            }
            let mut fourccs = vec![0 as EGLint; count.unsigned_abs() as usize];
            if query_formats(self.display, count, fourccs.as_mut_ptr(), &raw mut count) != 1 {
                return Vec::new();
            }
            fourccs.truncate(count.unsigned_abs() as usize);
            fourccs
        };

        fourccs
            .into_iter()
            .filter_map(|fourcc| {
                // SAFETY: as above.
                let enumerated =
                    unsafe { enumerate_modifiers(query_modifiers, self.display, fourcc) };
                advertisable_format(fourcc.cast_unsigned(), &enumerated)
            })
            .collect()
    }

    /// Import a dma-buf as an `EGLImage`.
    ///
    /// Failure is ordinary rather than exceptional: a client can describe a
    /// buffer this driver will not take, and the answer is to not draw it.
    pub fn import(&self, image: &DmabufImage) -> Result<EglImage, String> {
        let [plane] = image.planes.as_slice() else {
            return Err(format!(
                "{} plane(s): only single-plane images can be sampled by this renderer",
                image.planes.len()
            ));
        };
        if image.width <= 0 || image.height <= 0 {
            return Err(format!("empty image: {}x{}", image.width, image.height));
        }

        let mut attributes = vec![
            EGL_WIDTH,
            image.width,
            EGL_HEIGHT,
            image.height,
            EGL_LINUX_DRM_FOURCC_EXT,
            image.fourcc.cast_signed(),
            EGL_DMA_BUF_PLANE0_FD_EXT,
            plane.fd.as_raw_fd(),
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            plane.offset.cast_signed(),
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            plane.stride.cast_signed(),
        ];
        // An explicit modifier is only legal to pass when the driver
        // understands modifiers at all; without that extension the layout is
        // whatever the two sides agreed out of band.
        if image.modifier != DRM_FORMAT_MOD_INVALID && self.query.is_some() {
            attributes.extend_from_slice(&[
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                u32::try_from(image.modifier & 0xffff_ffff)
                    .unwrap_or(0)
                    .cast_signed(),
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                u32::try_from(image.modifier >> 32)
                    .unwrap_or(0)
                    .cast_signed(),
            ]);
        }
        attributes.push(EGL_NONE);

        // SAFETY: the attribute list is NONE-terminated and every descriptor in
        // it is alive for the call, held by the `DmabufImage` being imported.
        // EGL copies what it needs; the image does not borrow the list.
        let raw = unsafe {
            (self.create_image)(
                self.display,
                EGL_NO_CONTEXT,
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null(),
                attributes.as_ptr(),
            )
        };
        if raw == EGL_NO_IMAGE_KHR {
            return Err(format!(
                "driver refused a {}x{} {} buffer with modifier {:#x}",
                image.width,
                image.height,
                fourcc_name(image.fourcc),
                image.modifier,
            ));
        }
        Ok(EglImage {
            raw,
            display: self.display,
            destroy: self.destroy_image,
        })
    }

    /// Point the currently bound `TEXTURE_2D` at an imported image.
    ///
    /// # Safety
    /// A texture must be bound to `TEXTURE_2D` on the current context, and the
    /// image must outlive every draw that samples it.
    pub unsafe fn bind_to_texture(&self, image: &EglImage) {
        // SAFETY: delegated to this function's own contract.
        unsafe { (self.image_target_texture)(glow::TEXTURE_2D, image.raw) };
    }

    /// Check the import path end to end: export a texture the compositor made
    /// as a dma-buf, import that back, and read the pixels out again.
    ///
    /// Advertising dma-buf on the strength of an extension string is how a
    /// compositor ends up telling clients to allocate buffers it then cannot
    /// draw. This is the difference between "the entry points are there" and
    /// "it works", and it costs one 4x4 texture at startup.
    ///
    /// # Safety
    /// The GL context must be current on this thread, and `gl` must be its
    /// loaded entry points.
    pub unsafe fn self_test(&self, gl: &glow::Context) -> DmabufProbe {
        let (Some((export_query, export)), Some(current_context)) =
            (self.export, self.current_context)
        else {
            return DmabufProbe::Untested(
                "EGL_MESA_image_dma_buf_export is not available, so no dma-buf could be made to \
                 test the import with"
                    .into(),
            );
        };
        // SAFETY: delegated to this function's own contract; every GL object
        // created below is deleted before returning.
        unsafe {
            match self.round_trip(gl, export_query, export, current_context) {
                Ok(()) => DmabufProbe::Passed,
                Err(e) => DmabufProbe::Failed(e),
            }
        }
    }

    /// One export-and-import round trip, tearing down what it created.
    ///
    /// # Safety
    /// As [`Self::self_test`].
    unsafe fn round_trip(
        &self,
        gl: &glow::Context,
        export_query: PfnExportDmabufQueryMesa,
        export: PfnExportDmabufMesa,
        current_context: PfnGetCurrentContext,
    ) -> Result<(), String> {
        // A pattern with no repeats: every pixel differs in every channel, so
        // a transposed, sheared or offset readback cannot compare equal.
        let source: Vec<u8> = (0..SELF_TEST_SIDE * SELF_TEST_SIDE)
            .flat_map(|i| {
                let i = i.rem_euclid(256).unsigned_abs().to_le_bytes()[0];
                [
                    i.wrapping_mul(17),
                    i.wrapping_mul(29),
                    i.wrapping_mul(53),
                    0xff,
                ]
            })
            .collect();

        // SAFETY: the context is current, per this function's contract. The
        // texture is deleted on every path out.
        unsafe {
            let texture = gl
                .create_texture()
                .map_err(|e| format!("create_texture: {e}"))?;
            let result =
                self.round_trip_from(gl, texture, &source, export_query, export, current_context);
            gl.delete_texture(texture);
            result
        }
    }

    /// The round trip proper, given a texture to fill, export and re-import.
    ///
    /// # Safety
    /// As [`Self::self_test`], and `texture` must be a live texture name.
    unsafe fn round_trip_from(
        &self,
        gl: &glow::Context,
        texture: glow::Texture,
        source: &[u8],
        export_query: PfnExportDmabufQueryMesa,
        export: PfnExportDmabufMesa,
        current_context: PfnGetCurrentContext,
    ) -> Result<(), String> {
        // SAFETY: delegated to this function's own contract.
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST.cast_signed(),
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST.cast_signed(),
            );
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8.cast_signed(),
                SELF_TEST_SIDE,
                SELF_TEST_SIDE,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(source)),
            );
            // The export hands over memory, not a promise of pending work.
            gl.finish();

            let described = self.export_texture(texture, export_query, export, current_context)?;
            let reimported = self
                .import(&described)
                .map_err(|e| format!("re-importing what was just exported failed: {e}"))?;
            self.read_back(gl, &reimported, source)
        }
    }

    /// Export a GL texture as a dma-buf, described the way a client would.
    ///
    /// # Safety
    /// As [`Self::self_test`], and `texture` must be a live texture name whose
    /// contents have been flushed.
    unsafe fn export_texture(
        &self,
        texture: glow::Texture,
        export_query: PfnExportDmabufQueryMesa,
        export: PfnExportDmabufMesa,
        current_context: PfnGetCurrentContext,
    ) -> Result<DmabufImage, String> {
        // SAFETY: delegated to this function's own contract. The intermediate
        // image is destroyed by its own `Drop` on every path out, and the
        // exported descriptor is owned from the moment it is handed over.
        unsafe {
            let preserve = [EGL_IMAGE_PRESERVED_KHR, EGL_TRUE, EGL_NONE];
            let raw = (self.create_image)(
                self.display,
                current_context(),
                EGL_GL_TEXTURE_2D_KHR,
                std::ptr::without_provenance(texture.0.get() as usize),
                preserve.as_ptr(),
            );
            if raw == EGL_NO_IMAGE_KHR {
                return Err("could not make an EGLImage from a GL texture".into());
            }
            let exported = EglImage {
                raw,
                display: self.display,
                destroy: self.destroy_image,
            };

            let (mut fourcc, mut planes, mut modifier) = (0 as EGLint, 0 as EGLint, 0u64);
            if export_query(
                self.display,
                exported.raw,
                &raw mut fourcc,
                &raw mut planes,
                &raw mut modifier,
            ) != 1
            {
                return Err("eglExportDMABUFImageQueryMESA failed".into());
            }
            if planes != 1 {
                return Err(format!("exported image has {planes} planes, expected 1"));
            }

            let (mut fd, mut stride, mut offset) = (-1 as EGLint, 0 as EGLint, 0 as EGLint);
            if export(
                self.display,
                exported.raw,
                &raw mut fd,
                &raw mut stride,
                &raw mut offset,
            ) != 1
                || fd < 0
            {
                return Err("eglExportDMABUFImageMESA failed".into());
            }

            debug!(
                "dma-buf self-test: exported {SELF_TEST_SIDE}x{SELF_TEST_SIDE} {} modifier {modifier:#x} stride {stride}",
                fourcc_name(fourcc.cast_unsigned()),
            );

            Ok(DmabufImage {
                width: SELF_TEST_SIDE,
                height: SELF_TEST_SIDE,
                fourcc: fourcc.cast_unsigned(),
                modifier,
                planes: vec![DmabufPlane {
                    // Owned from here on, so it is closed when the image is
                    // dropped however this ends.
                    fd: Arc::new(OwnedFd::from_raw_fd(fd)),
                    offset: offset.unsigned_abs(),
                    stride: stride.unsigned_abs(),
                }],
            })
        }
    }

    /// Bind an imported image to a texture, read it back, and compare.
    ///
    /// # Safety
    /// As [`Self::self_test`].
    unsafe fn read_back(
        &self,
        gl: &glow::Context,
        image: &EglImage,
        expected: &[u8],
    ) -> Result<(), String> {
        // SAFETY: the context is current, and both objects are deleted before
        // returning on every path.
        unsafe {
            let texture = gl
                .create_texture()
                .map_err(|e| format!("create_texture: {e}"))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST.cast_signed(),
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST.cast_signed(),
            );
            self.bind_to_texture(image);

            // Reading a texture back means rendering from it: GLES has no
            // glGetTexImage, so it is attached to a framebuffer and read there.
            let framebuffer = match gl.create_framebuffer() {
                Ok(fb) => fb,
                Err(e) => {
                    gl.delete_texture(texture);
                    return Err(format!("create_framebuffer: {e}"));
                }
            };
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );

            let mut readback = vec![0u8; expected.len()];
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            let result = if status == glow::FRAMEBUFFER_COMPLETE {
                gl.read_pixels(
                    0,
                    0,
                    SELF_TEST_SIDE,
                    SELF_TEST_SIDE,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut readback)),
                );
                if readback == expected {
                    Ok(())
                } else {
                    Err(format!(
                        "imported pixels differ from what was exported: {:02x?} vs {:02x?}",
                        &readback[..8.min(readback.len())],
                        &expected[..8.min(expected.len())],
                    ))
                }
            } else {
                Err(format!(
                    "cannot read back the imported image: framebuffer status {status:#x}"
                ))
            };

            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_framebuffer(framebuffer);
            gl.delete_texture(texture);
            result
        }
    }
}

/// Ask the driver for one format's modifiers, each paired with whether it is
/// `external_only`.
///
/// An empty answer covers both ways of saying nothing — a driver that will not
/// enumerate, and one that enumerates none — because they mean the same thing:
/// the implicit layout is all there is. See [`advertisable_format`], which is
/// where that is acted on.
///
/// # Safety
/// `query` must be `eglQueryDmaBufModifiersEXT` for `display`.
unsafe fn enumerate_modifiers(
    query: PfnQueryDmabufModifiers,
    display: EGLDisplay,
    fourcc: EGLint,
) -> Vec<(u64, bool)> {
    // SAFETY: delegated to this function's own contract. Each call passes a
    // count matching the buffers handed over: first none, to learn the count,
    // then exactly that many.
    unsafe {
        let mut count: EGLint = 0;
        if query(
            display,
            fourcc,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut count,
        ) != 1
            || count <= 0
        {
            return Vec::new();
        }

        let len = count.unsigned_abs() as usize;
        let mut modifiers = vec![0u64; len];
        let mut external = vec![0 as EGLBoolean; len];
        if query(
            display,
            fourcc,
            count,
            modifiers.as_mut_ptr(),
            external.as_mut_ptr(),
            &raw mut count,
        ) != 1
        {
            return Vec::new();
        }
        modifiers.truncate(count.unsigned_abs() as usize);

        modifiers
            .into_iter()
            .zip(external)
            .map(|(modifier, external_only)| (modifier, external_only != 0))
            .collect()
    }
}

/// Whether a format can be offered to clients, and with which modifiers.
///
/// This is the rule that keeps the compositor from advertising what it cannot
/// draw, which matters more than it sounds: a client allocates against the list
/// it is given, and a buffer it allocated for a format we then refuse leaves it
/// with nothing to fall back to.
///
/// Two things get filtered. A modifier marked `external_only` needs
/// `samplerExternalOES`, which this renderer has no program for. And a format
/// whose every modifier was external is dropped outright — on Mesa that is
/// every YUV layout, which is why the advertised list is much shorter than the
/// driver's.
///
/// An empty `enumerated` is the opposite case and must not be confused with it:
/// the driver named no modifiers, so the implicit layout is all there is, and
/// that imports fine. "Nothing named" and "nothing usable" look alike and mean
/// opposite things.
fn advertisable_format(fourcc: u32, enumerated: &[(u64, bool)]) -> Option<DmabufFormat> {
    if enumerated.is_empty() {
        return Some(DmabufFormat {
            fourcc,
            modifiers: Vec::new(),
        });
    }

    let modifiers: Vec<u64> = enumerated
        .iter()
        .filter(|&&(_, external_only)| !external_only)
        .map(|&(modifier, _)| modifier)
        .collect();
    if modifiers.is_empty() {
        return None;
    }
    Some(DmabufFormat { fourcc, modifiers })
}

/// Resolve one entry point, or `None` if the driver does not have it.
///
/// # Safety
/// `T` must be the function pointer type the symbol actually has. Getting that
/// wrong is a call through a mistyped pointer, which is why every use sits next
/// to the extension spec the signature came from.
unsafe fn load<T: Copy>(loader: &dyn Fn(&CStr) -> *const c_void, symbol: &CStr) -> Option<T> {
    const {
        assert!(
            size_of::<T>() == size_of::<*const c_void>(),
            "entry points are function pointers"
        );
    }
    let address = loader(symbol);
    if address.is_null() {
        return None;
    }
    // SAFETY: the size is checked above and the signature is the caller's
    // promise; `address` is non-null and stays valid for the life of the
    // display it was loaded from.
    Some(unsafe { *std::ptr::from_ref(&address).cast::<T>() })
}

/// Whether an extension is in a space-separated EGL extension string.
///
/// Substring matching would say yes to `EGL_EXT_image_dma_buf_import` for a
/// driver offering only `EGL_EXT_image_dma_buf_import_modifiers`, which is a
/// different extension with different entry points.
fn has_extension(extensions: &str, wanted: &str) -> bool {
    extensions.split_whitespace().any(|e| e == wanted)
}

#[cfg(test)]
mod tests;
