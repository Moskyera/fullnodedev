use std::path::Path;

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions, Vec2};

const EMBEDDED_LOGO: &[u8] = include_bytes!("../assets/hhh.png");

pub fn load_logo(ctx: &egui::Context, work_dir: &Path) -> Option<TextureHandle> {
    let external = work_dir.join("hhh.png");
    if external.is_file() {
        if let Ok(bytes) = std::fs::read(&external) {
            if let Some(tex) = texture_from_png(ctx, &bytes, "app_logo") {
                return Some(tex);
            }
        }
    }
    texture_from_png(ctx, EMBEDDED_LOGO, "app_logo")
}

fn texture_from_png(ctx: &egui::Context, bytes: &[u8], name: &str) -> Option<TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?;
    let mut rgba = img.to_rgba8();
    strip_near_white_background(&mut rgba);
    let size = [rgba.width() as usize, rgba.height() as usize];
    let color_image = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Some(ctx.load_texture(name, color_image, TextureOptions::LINEAR))
}

/// Turn solid/near-white PNG backgrounds transparent (keeps soft edges on the logo).
fn strip_near_white_background(rgba: &mut image::RgbaImage) {
    const HARD_CUTOFF: f32 = 248.0;
    const SOFT_START: f32 = 220.0;

    for pixel in rgba.pixels_mut() {
        let [r, g, b, a] = pixel.0;
        if a == 0 {
            continue;
        }
        let lum = (r as f32 + g as f32 + b as f32) / 3.0;
        if lum >= HARD_CUTOFF {
            pixel.0[3] = 0;
        } else if lum > SOFT_START {
            let fade = (HARD_CUTOFF - lum) / (HARD_CUTOFF - SOFT_START);
            pixel.0[3] = ((a as f32) * fade.clamp(0.0, 1.0)) as u8;
        }
    }
}

pub fn logo_size_for_height(texture: &TextureHandle, height: f32) -> Vec2 {
    let native = texture.size_vec2();
    if native.y <= 0.0 {
        return Vec2::new(height, height);
    }
    Vec2::new(height * native.x / native.y, height)
}