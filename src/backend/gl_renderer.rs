//! GL scene rasteriser.
//!
//! Draws a `Scene` as textured quads through GLES 3.0, and caches one GPU
//! texture per `TextureId` so a surface that has not been redrawn costs no
//! upload. Must be used only on the thread that owns the GL context (the
//! backend thread.)
//!
//! Client pixels arrive as little-endian `0xAARRGGBB`, i.e. `[B, G, R, A]`.
//! GLES has no guaranteed BGRA upload format, so the bytes go up as `RGBA`
//! untouched and the fragment shader swizzles — no extension needed, and no
//! second pass over the pixels on the CPU.

use std::collections::HashMap;

use glow::HasContext;
use tracing::{debug, warn};

use crate::shared::{
    BACKGROUND_COLOR, Frame, PixelFormat, Scene, TextureId, TextureImage, TextureRect,
};

/// Vertex shader: puts one textured quad in place.
///
/// Runs once per vertex, so four times per element — once for each corner of
/// `QUAD`. Its job is to stretch that unit square into a rectangle at the
/// right spot on screen and hand the fragment shader the matching corner of
/// the source crop; the GPU interpolates `v_texcoord` across everything in
/// between for free.
///
/// The geometry itself never changes, which is the point: the same four
/// vertices are drawn for every element and only the uniforms move. `a_unit`
/// of `(0, 0)` is the top-left of both rectangles and `(1, 1)` the
/// bottom-right, so one corner value indexes destination and source alike —
/// `mix` is component-wise, so x picks between `u_src`'s two u values and y
/// between its two v values.
///
/// `gl_Position` has to come back in clip space: x and y in -1..1 with the
/// origin at the centre of the window, so pixels are divided by the viewport
/// and rescaled. The y negation is the only twist, and it is why the source
/// coordinates need no flip of their own — the texture rows are already
/// stored top-down, and flipping the quad instead lines the two up.
const VERTEX_SHADER: &str = r"#version 300 es
precision highp float;

// A unit quad, scaled into place by the destination rectangle. Working in
// pixels and converting here keeps the scene in one coordinate space.
layout(location = 0) in vec2 a_unit;

// Destination rect in output pixels: (x, y, width, height).
uniform vec4 u_dst;
// Output size in pixels, for the pixels-to-clip-space conversion.
uniform vec2 u_viewport;
// Source rect in normalised texture coordinates: (u0, v0, u1, v1).
uniform vec4 u_src;

out vec2 v_texcoord;

void main() {
    vec2 pixel = u_dst.xy + a_unit * u_dst.zw;
    vec2 ndc = (pixel / u_viewport) * 2.0 - 1.0;
    // Wayland's y axis grows downward, GL's grows upward.
    gl_Position = vec4(ndc.x, -ndc.y, 0.0, 1.0);
    v_texcoord = mix(u_src.xy, u_src.zw, a_unit);
}
";

/// Fragment shader: colours one pixel of that quad.
///
/// Runs once per pixel the quad covers, with `v_texcoord` interpolated from
/// the four corners the vertex shader emitted. All it does is sample the
/// surface texture there and repair the two ways a client buffer differs from
/// what GL assumes: the channel order described in the module header, and
/// XRGB8888's undefined high byte.
///
/// What it writes goes straight into the blend stage set up in `new`
/// (`ONE, ONE_MINUS_SRC_ALPHA`), so the output must stay premultiplied —
/// hence only the alpha is touched, never the colour channels.
///
/// `u_ignore_alpha` is a float rather than a bool so the fix can be a `mix`:
/// both formats then run the same instructions with no per-pixel branch.
const FRAGMENT_SHADER: &str = r"#version 300 es
precision highp float;

in vec2 v_texcoord;
uniform sampler2D u_texture;
// 1.0 when the source has no alpha channel (XRGB8888), 0.0 otherwise.
uniform float u_ignore_alpha;

out vec4 f_color;

void main() {
    // Uploaded as RGBA but laid out [B, G, R, A], so swizzle back.
    vec4 texel = texture(u_texture, v_texcoord).bgra;
    // XRGB8888's high byte is undefined; force it opaque. The colour channels
    // are already premultiplied in both cases, so nothing else changes.
    f_color = vec4(texel.rgb, mix(texel.a, 1.0, u_ignore_alpha));
}
";

/// Unit quad as a triangle strip, in the order the vertex shader mixes
/// source coordinates: top-left, top-right, bottom-left, bottom-right.
const QUAD: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];

/// A cached texture
struct CachedTexture {
    /// The texture
    texture: glow::Texture,
    /// Serial of the image last uploaded, so unchanged content is not re-sent,
    /// and so a partial update can check it is patching what it thinks it is.
    serial: u64,
    /// Width of the texture
    width: i32,
    /// Height of the texture
    height: i32,
}

/// The whole GL pipeline: one program, one quad, and the texture cache.
///
/// Everything here is built once in `new` and reused for every element of
/// every frame. Drawing a scene is then a matter of rebinding a texture and
/// setting uniforms per element, so the per-frame cost scales with the number
/// of surfaces rather than with their size.
///
/// Every field is a handle into the GL context, valid only while that context
/// is current on this thread. That is why the type is not `Send` in practice
/// and why `new` is unsafe: the caller promises the context outlives the
/// renderer and stays current. `Drop` gives the handles back in the same
/// breath, so the context must still be current when the renderer is dropped.
pub struct GlRenderer {
    /// Loaded GL entry points. Owning it here ties every handle below to the
    /// context they were created in.
    gl: glow::Context,
    /// The linked vertex + fragment program from the shaders above. There is
    /// only one, because there is only one kind of thing to draw.
    program: glow::Program,
    /// Vertex array holding the attribute layout for `QUAD`, so drawing is a
    /// single bind rather than a re-description of the format each time.
    vao: glow::VertexArray,
    /// Buffer holding `QUAD` itself. Never touched after upload, but kept so
    /// `Drop` can delete it.
    vbo: glow::Buffer,
    /// Destination rect uniform, in output pixels.
    u_dst: Option<glow::UniformLocation>,
    /// Output size uniform, set once per `draw` rather than per element.
    u_viewport: Option<glow::UniformLocation>,
    /// Source crop uniform, in normalised texture coordinates.
    u_src: Option<glow::UniformLocation>,
    /// Opaque-alpha flag uniform, 1.0 for XRGB8888.
    ///
    /// All four are `Option` because GL returns no location for a uniform the
    /// linker optimised out; a `None` simply makes the later set a no-op
    /// instead of an error.
    u_ignore_alpha: Option<glow::UniformLocation>,
    /// One GPU texture per `TextureId`, so a surface that has not changed
    /// costs no upload. Entries outlive the frame that created them and are
    /// pruned by `drop_unused_cached_textures`.
    textures: HashMap<TextureId, CachedTexture>,
}

impl GlRenderer {
    /// Build the pipeline. `loader` resolves GL function pointers, and the
    /// caller must have made the context current first.
    ///
    /// # Safety
    /// `loader` must return valid GL entry points for the current context, and
    /// that context must stay current for as long as the renderer is used.
    pub unsafe fn new(
        loader: impl FnMut(&std::ffi::CStr) -> *const std::ffi::c_void,
    ) -> anyhow::Result<Self> {
        let gl = unsafe { glow::Context::from_loader_function_cstr(loader) };

        unsafe {
            let program = link_program(&gl)?;
            gl.use_program(Some(program));

            let u_dst = gl.get_uniform_location(program, "u_dst");
            let u_viewport = gl.get_uniform_location(program, "u_viewport");
            let u_src = gl.get_uniform_location(program, "u_src");
            let u_ignore_alpha = gl.get_uniform_location(program, "u_ignore_alpha");
            if let Some(loc) = gl.get_uniform_location(program, "u_texture") {
                gl.uniform_1_i32(Some(&loc), 0);
            }

            let vao = gl
                .create_vertex_array()
                .map_err(|e| anyhow::anyhow!("failed to create vertex array: {e}"))?;
            gl.bind_vertex_array(Some(vao));

            let vbo = gl
                .create_buffer()
                .map_err(|e| anyhow::anyhow!("failed to create vertex buffer: {e}"))?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&QUAD), glow::STATIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);

            // Every source we draw is premultiplied: shm ARGB8888 by protocol,
            // XRGB8888 trivially, and both cursor paths by construction.
            gl.enable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            // Rows are tightly packed at width * 4, not aligned to 4-pixel
            // boundaries, so the default unpack alignment of 4 is wrong.
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);

            debug!(
                "GL renderer ready: {} / {}",
                gl.get_parameter_string(glow::RENDERER),
                gl.get_parameter_string(glow::VERSION)
            );

            Ok(Self {
                gl,
                program,
                vao,
                vbo,
                u_dst,
                u_viewport,
                u_src,
                u_ignore_alpha,
                textures: HashMap::new(),
            })
        }
    }

    /// Draw a scene into the currently bound framebuffer.
    ///
    /// `width`/`height` are the real drawable size, which can differ from the
    /// scene's own size for a frame or two while a resize is in flight; the
    /// scene is drawn at its own coordinates and any surplus shows background.
    pub fn draw(&mut self, scene: &Scene, width: u32, height: u32) {
        unsafe {
            let gl = &self.gl;
            gl.viewport(0, 0, width.cast_signed(), height.cast_signed());
            let [r, g, b, a] = unpack_color(BACKGROUND_COLOR);
            gl.clear_color(r, g, b, a);
            gl.clear(glow::COLOR_BUFFER_BIT);

            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.uniform_2_f32(
                self.u_viewport.as_ref(),
                as_f32(width.max(1)),
                as_f32(height.max(1)),
            );
        }

        for element in &scene.elements {
            let image = &element.texture;
            // Uploading may create or replace a texture, so it happens before
            // the borrow of `self.gl` used to draw with it.
            let Some(texture) = self.upload(image) else {
                continue;
            };
            let gl = &self.gl;
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));

                let (dx, dy, dw, dh) = element.dst;
                gl.uniform_4_f32(
                    self.u_dst.as_ref(),
                    as_f32(dx),
                    as_f32(dy),
                    as_f32(dw),
                    as_f32(dh),
                );

                // Normalise the source crop against the texture it came from.
                let (tw, th) = (f64::from(image.width), f64::from(image.height));
                let (sx, sy, sw, sh) = element.src;
                gl.uniform_4_f32(
                    self.u_src.as_ref(),
                    as_f32(sx / tw),
                    as_f32(sy / th),
                    as_f32((sx + sw) / tw),
                    as_f32((sy + sh) / th),
                );

                let ignore_alpha = f32::from(u8::from(image.format == PixelFormat::Xrgb8888));
                gl.uniform_1_f32(self.u_ignore_alpha.as_ref(), ignore_alpha);

                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }
        }

        unsafe { self.gl.bind_vertex_array(None) };
    }

    /// Return the GPU texture for an image, uploading whatever part of it the
    /// GPU does not already have.
    ///
    /// A full upload happens when there is nothing to patch: no cached texture,
    /// one at a different serial than this image was derived from, a change of
    /// dimensions, or damage the compositor could not describe. Otherwise only
    /// the damaged rectangles go up, which for a client redrawing a small part
    /// of a large window is the difference between megabytes and kilobytes per
    /// frame.
    fn upload(&mut self, image: &TextureImage) -> Option<glow::Texture> {
        let cached = self.textures.get(&image.id);
        if let Some(cached) = cached
            && cached.serial == image.serial
        {
            return Some(cached.texture);
        }

        let patchable = cached.is_some_and(|cached| {
            Some(cached.serial) == image.previous_serial
                && cached.width == image.width
                && cached.height == image.height
        }) && !image.damage.is_empty();

        let gl = &self.gl;
        unsafe {
            if patchable {
                let texture = self.textures[&image.id].texture;
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                for rect in &image.damage {
                    upload_region(gl, image, *rect);
                }
                self.textures.insert(
                    image.id,
                    CachedTexture {
                        texture,
                        serial: image.serial,
                        width: image.width,
                        height: image.height,
                    },
                );
                return Some(texture);
            }

            // Dimensions may have changed, so reallocate rather than trying to
            // sub-image into storage of the wrong size.
            if let Some(old) = self.textures.remove(&image.id) {
                gl.delete_texture(old.texture);
            }

            let texture = match gl.create_texture() {
                Ok(t) => t,
                Err(e) => {
                    warn!("failed to create texture: {e}");
                    return None;
                }
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR.cast_signed(),
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR.cast_signed(),
            );
            // Surfaces are drawn at their own size or cropped by a viewport, so
            // sampling never wants to repeat; clamping also stops linear
            // filtering pulling in the opposite edge.
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE.cast_signed(),
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE.cast_signed(),
            );
            // SAFETY: the image is alive for this call, so the client cannot
            // yet have been told it may draw into the buffer again.
            let Some(bytes) = image.bytes() else {
                warn!("image {:?} does not fit its mapping", image.id);
                gl.delete_texture(texture);
                return None;
            };
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, image.row_length());
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8.cast_signed(),
                image.width,
                image.height,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(bytes)),
            );
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);

            self.textures.insert(
                image.id,
                CachedTexture {
                    texture,
                    serial: image.serial,
                    width: image.width,
                    height: image.height,
                },
            );
            Some(texture)
        }
    }

    /// Drop the cached texture for every id the frame did not reference.
    ///
    /// Buffers and cursors are destroyed compositor-side without the backend
    /// hearing about it, so the cache is trimmed against what is actually still
    /// being drawn rather than by an explicit eviction message.
    ///
    /// Takes the whole frame rather than one scene: with more than one output
    /// the same texture can appear in one scene and not another, and evicting
    /// per scene would drop and re-upload it on every frame.
    pub fn drop_unused_cached_textures(&mut self, frame: &Frame) {
        let live: std::collections::HashSet<TextureId> = frame
            .iter()
            .flat_map(|scene| scene.elements.iter())
            .map(|e| e.texture.id)
            .collect();
        let gl = &self.gl;
        self.textures.retain(|id, cached| {
            if live.contains(id) {
                return true;
            }
            unsafe { gl.delete_texture(cached.texture) };
            false
        });
    }
}

impl Drop for GlRenderer {
    /// Drop implementation for this `GlRenderer`, cleans up Gl resources
    fn drop(&mut self) {
        unsafe {
            for cached in self.textures.values() {
                self.gl.delete_texture(cached.texture);
            }
            self.gl.delete_buffer(self.vbo);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_program(self.program);
        }
    }
}

/// Creates and links a GL program with shaders for rendering scenes from the
/// compositor.
unsafe fn link_program(gl: &glow::Context) -> anyhow::Result<glow::Program> {
    unsafe {
        let program = gl
            .create_program()
            .map_err(|e| anyhow::anyhow!("failed to create program: {e}"))?;

        let mut shaders = Vec::new();
        for (kind, source) in [
            (glow::VERTEX_SHADER, VERTEX_SHADER),
            (glow::FRAGMENT_SHADER, FRAGMENT_SHADER),
        ] {
            let shader = gl
                .create_shader(kind)
                .map_err(|e| anyhow::anyhow!("failed to create shader: {e}"))?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let log = gl.get_shader_info_log(shader);
                return Err(anyhow::anyhow!("shader failed to compile: {log}"));
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }

        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            return Err(anyhow::anyhow!("program failed to link: {log}"));
        }
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        Ok(program)
    }
}

/// Upload one damaged rectangle out of an image's full pixel buffer.
///
/// The rows of the rectangle are not contiguous in `image.pixels`, so
/// `UNPACK_ROW_LENGTH` tells GL the real row stride and the slice simply starts
/// at the rectangle's first pixel. Both are GLES 3.0 core, so this needs no
/// extension.
unsafe fn upload_region(gl: &glow::Context, image: &TextureImage, rect: TextureRect) {
    let row_length = image.row_length().unsigned_abs() as usize;
    // SAFETY: the image is alive for this call, so the client cannot yet have
    // been told it may draw into the buffer again.
    let Some(bytes) = (unsafe { image.bytes() }) else {
        warn!("image {:?} does not fit its mapping", image.id);
        return;
    };
    let start = (rect.y.unsigned_abs() as usize * row_length + rect.x.unsigned_abs() as usize) * 4;
    // Through to the end of the rectangle's last row; the trailing pixels of
    // that row are never read, because GL stops at `rect.width`.
    let len = ((rect.height.unsigned_abs() as usize - 1) * row_length
        + rect.width.unsigned_abs() as usize)
        * 4;
    let Some(pixels) = bytes.get(start..start + len) else {
        // Should be unreachable: the rectangle was clamped to the buffer when
        // the damage was recorded. Skipping leaves the region stale, which is
        // better than reading out of bounds.
        warn!("damage rectangle {rect:?} outside image {:?}", image.id);
        return;
    };

    unsafe {
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, image.row_length());
        gl.tex_sub_image_2d(
            glow::TEXTURE_2D,
            0,
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(pixels)),
        );
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
    }
}

/// Narrow a coordinate to the `f32` that GL uniforms take.
///
/// Everything passed here is either a pixel coordinate on a real display or a
/// normalised texture coordinate, so `f32`'s 23-bit mantissa is never the
/// limiting factor; the cast is the price of talking to GL at all.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn as_f32(value: impl Into<f64>) -> f32 {
    value.into() as f32
}

/// Split an `0xAARRGGBB` word into GL's normalised float channels.
fn unpack_color(argb: u32) -> [f32; 4] {
    let channel = |shift: u32| f32::from(((argb >> shift) & 0xff) as u8) / 255.0;
    [channel(16), channel(8), channel(0), channel(24)]
}

/// Reinterpret the quad's floats as the bytes `buffer_data_u8_slice` wants.
fn bytemuck_cast(floats: &[f32; 8]) -> &[u8] {
    // SAFETY: `f32` has no padding or invalid bit patterns, and the result
    // borrows from `floats`, so the lifetime and size are exact.
    unsafe {
        std::slice::from_raw_parts(floats.as_ptr().cast::<u8>(), std::mem::size_of_val(floats))
    }
}
