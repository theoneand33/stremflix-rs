//! Posters and backdrops drawn from the cache, fetched on first sight.

use crate::*;

impl App {
    pub(crate) fn fetch_poster(&mut self, ctx: &egui::Context, key: String) {
        // Several cards can ask for one image in the same frame.
        if self.posters.get(&key).is_some_and(|p| p.loading) {
            return;
        }
        let ppp = self.poster_ppp.max(1.0);
        // A resize keeps the old texture on screen until the sharper one arrives.
        let old = self.posters.remove(&key);
        let (tex, bytes) = old.map_or((None, 0), |p| (p.tex, p.bytes));
        self.posters.insert(key.clone(), Poster { tex, failed: false, bytes, loading: true, asked: ppp, used: Cell::new(self.poster_frame) });
        let (client, slots) = (self.client.clone(), self.poster_slots.clone());
        let px = |side: f32| (side * GROW * ppp).ceil() as u32;
        let target = key.starts_with(CARD_KEY).then(|| [px(CARD.x), px(CARD.y)]);
        self.spawn(ctx, async move {
            let image = async {
                let body = client.poster(key.strip_prefix(CARD_KEY).unwrap_or(&key)).await?;
                let _slot = slots.acquire_owned().await?;
                tokio::task::spawn_blocking(move || decode_poster(&body, target)).await?
            };
            let image = image.await.ok();
            Ok(Msg::Poster(key, ppp, image))
        });
    }

    /// Full-size poster/background if loaded; queues a fetch on first sight.
    pub(crate) fn image(&self, url: &Option<String>, want: &mut Vec<String>) -> Option<egui::load::SizedTexture> {
        self.texture(url.as_ref()?, want)
    }

    /// Like [`Self::image`], shrunk to card size.
    pub(crate) fn card_image(&self, url: &Option<String>, want: &mut Vec<String>) -> Option<egui::load::SizedTexture> {
        self.texture(&format!("{CARD_KEY}{}", url.as_ref()?), want)
    }

    pub(crate) fn texture(&self, key: &str, want: &mut Vec<String>) -> Option<egui::load::SizedTexture> {
        let Some(p) = self.posters.get(key) else {
            want.push(key.to_owned());
            return None;
        };
        p.used.set(self.poster_frame);
        // Sized for a lower scale: ask for a sharper one, and draw this meanwhile.
        if !p.loading && key.starts_with(CARD_KEY) && p.asked < self.poster_ppp.max(1.0) {
            want.push(key.to_owned());
        }
        p.tex.as_ref().map(egui::load::SizedTexture::from_handle)
    }
}
