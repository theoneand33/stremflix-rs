//! The poster and backdrop cache: images decoded off the UI thread, card posters sized for the display scale.

use eframe::egui::{self, pos2};
use std::cell::Cell;

/// Poster cache limits; the least recently drawn go first once either is passed.
pub(crate) const POSTER_CACHE: usize = 1500;

pub(crate) const POSTER_BYTES: usize = 384 << 20;

/// Cache key prefix of a poster shrunk to card size; other keys are plain URLs, kept at full size.
pub(crate) const CARD_KEY: &str = "card:";

/// A fetched image. `tex` is `None` while first loading and after a failure (`failed`); a card poster being resized keeps its old texture until the new one lands.
pub(crate) struct Poster {
    pub(crate) tex: Option<egui::TextureHandle>,
    pub(crate) failed: bool,
    pub(crate) bytes: usize,
    /// A fetch is in flight.
    pub(crate) loading: bool,
    /// Scale the latest fetch was sized for; below the current one, a card poster is refetched.
    pub(crate) asked: f32,
    /// Frame it was last drawn on.
    pub(crate) used: Cell<u64>,
}

/// Keys to drop, least recently used first, to get 10% under the limits. Entries drawn on `frame` stay, so what is on screen never thrashes.
pub(crate) fn posters_to_evict(mut entries: Vec<(u64, usize, String)>, frame: u64, max_len: usize, max_bytes: usize) -> Vec<String> {
    let (mut len, mut bytes) = (entries.len(), entries.iter().map(|e| e.1).sum::<usize>());
    if len <= max_len && bytes <= max_bytes {
        return Vec::new();
    }
    entries.retain(|e| e.0 < frame);
    entries.sort_unstable_by_key(|e| e.0);
    let mut out = Vec::new();
    for (_, b, key) in entries {
        if len <= max_len / 10 * 9 && bytes <= max_bytes / 10 * 9 {
            break;
        }
        (len, bytes) = (len - 1, bytes - b);
        out.push(key);
    }
    out
}

/// Decode an image, shrunk to just cover `target` pixels if given and smaller. CPU-heavy: call off the UI thread.
pub(crate) fn decode_poster(body: &[u8], target: Option<[u32; 2]>) -> anyhow::Result<egui::ColorImage> {
    use image::ImageDecoder;
    let mut decoder = image::ImageReader::new(std::io::Cursor::new(body)).with_guessed_format()?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut img = image::DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    if let Some([w, h]) = target {
        let scale = (w as f32 / img.width() as f32).max(h as f32 / img.height() as f32);
        if scale < 1.0 {
            let size = |side: u32| ((side as f32 * scale).ceil() as u32).max(1);
            img = img.resize_exact(size(img.width()), size(img.height()), image::imageops::FilterType::CatmullRom);
        }
    }
    let rgba = img.to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

/// Cover-fit UVs: crop an image of `size` to `rect`'s aspect ratio, centered.
pub(crate) fn cover_uv(rect: egui::Rect, size: egui::Vec2) -> egui::Rect {
    let (ra, ia) = (rect.aspect_ratio(), size.x / size.y);
    if ia > ra {
        let f = ra / ia;
        egui::Rect::from_min_max(pos2((1.0 - f) / 2.0, 0.0), pos2((1.0 + f) / 2.0, 1.0))
    } else {
        let f = ia / ra;
        egui::Rect::from_min_max(pos2(0.0, (1.0 - f) / 2.0), pos2(1.0, (1.0 + f) / 2.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recent_posters_but_not_this_frames() {
        let e = |used, bytes, k: &str| (used, bytes, k.to_string());
        // Under both limits: nothing goes.
        assert!(posters_to_evict(vec![e(1, 10, "a")], 5, 10, 100).is_empty());
        // Over bytes: oldest first, stopping 10% under the limit.
        let got = posters_to_evict(vec![e(3, 60, "c"), e(1, 60, "a"), e(2, 60, "b")], 5, 10, 100);
        assert_eq!(got, ["a", "b"]);
        // Everything on screen this frame stays even past the limit.
        assert!(posters_to_evict(vec![e(5, 200, "a")], 5, 10, 100).is_empty());
        // Over count, with empty (failed or loading) entries.
        let got = posters_to_evict((0..12).map(|i| e(i, 0, &format!("{i:02}"))).collect(), 99, 10, 100);
        assert_eq!(got, ["00", "01", "02"]);
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(w, h).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn decode_shrinks_to_cover_the_target_only() {
        assert_eq!(decode_poster(&png(400, 600), Some([100, 100])).unwrap().size, [100, 150], "keeps the aspect ratio, covers both sides");
        assert_eq!(decode_poster(&png(50, 75), Some([100, 100])).unwrap().size, [50, 75], "never upscales");
        assert_eq!(decode_poster(&png(400, 600), None).unwrap().size, [400, 600], "no target keeps full size");
        assert_eq!(decode_poster(&png(4000, 1), Some([100, 1])).unwrap().size, [4000, 1], "covers the tighter side");
        assert!(decode_poster(b"not an image", None).is_err());
    }
}
