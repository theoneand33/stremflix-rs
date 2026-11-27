//! The framebuffer libmpv renders into, shown as an egui texture.

use eframe::egui;
use eframe::glow::{self, HasContext};
use stremflix_rs::player::Player;

/// The framebuffer mpv renders into, its texture (owned by egui once registered) and pixel size; kept across playbacks.
pub(crate) struct VideoSurface {
    pub(crate) fbo: glow::Framebuffer,
    pub(crate) tex: glow::Texture,
    pub(crate) id: egui::TextureId,
    pub(crate) size: [usize; 2],
}

impl VideoSurface {
    pub(crate) fn new(gl: &glow::Context, frame: &mut eframe::Frame) -> Result<Self, String> {
        // SAFETY: called from `ui` with eframe's GL context current.
        unsafe {
            match (gl.create_framebuffer(), gl.create_texture()) {
                (Ok(fbo), Ok(tex)) => Ok(Self { fbo, tex, id: frame.register_native_glow_texture(tex), size: [0, 0] }),
                (a, b) => Err(format!("GL setup failed: {:?}", a.err().or(b.err()))),
            }
        }
    }

    /// The next render redraws even without a new frame, so the previous video's last frame never flashes.
    pub(crate) fn invalidate(&mut self) {
        self.size = [0, 0];
    }

    /// Renders `player`'s frame at `size` pixels and leaves the GL state as egui's painter expects; the texture to draw.
    pub(crate) fn render(&mut self, gl: &glow::Context, player: &mut Player, size: [usize; 2]) -> egui::TextureId {
        let resized = self.size != size;
        // SAFETY: called from `ui` with eframe's GL context current; the framebuffer and texture are ours.
        unsafe {
            if resized {
                gl.bind_texture(glow::TEXTURE_2D, Some(self.tex));
                // The default min filter wants mipmaps; without them the texture samples black.
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, size[0] as i32, size[1] as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(self.tex), 0);
                gl.bind_texture(glow::TEXTURE_2D, None);
            }
            // egui leaves scissor/blend enabled; mpv draws assuming they are off.
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            player.render(self.fbo.0.get(), size, resized);
            // Undo the GL state mpv leaves behind that egui's painter assumes is default.
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        }
        self.size = size;
        self.id
    }
}
