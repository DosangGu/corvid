use anyhow::{Context, Result, anyhow};
use corvid_deck::{Frame, KEY_COUNT, KeyFace, Tone, art, live::Face};
use image::{DynamicImage, RgbaImage};
use resvg::{tiny_skia, usvg};

use super::usb::{KEY_IMAGE_SIZE, StreamDeck};

pub struct Renderer {
    options: usvg::Options<'static>,
}

impl Renderer {
    pub fn new() -> Self {
        let mut options = usvg::Options::default();
        options.fontdb_mut().load_system_fonts();
        Self { options }
    }

    pub fn present(
        &self,
        device: &StreamDeck,
        previous: Option<&Frame>,
        next: &Frame,
    ) -> Result<()> {
        let images = changes(previous, next)
            .map(|(index, face)| self.render(face).map(|image| (index, image)));
        device
            .write_key_images(images)
            .context("send key images to Stream Deck")
    }

    fn render(&self, face: KeyFace) -> Result<DynamicImage> {
        self.render_tile(
            face.key.glyph,
            face.key.label,
            "",
            face.key.tone,
            face.highlighted(),
            face.enabled,
        )
    }

    pub fn present_live(
        &self,
        device: &StreamDeck,
        previous: Option<&[Face; KEY_COUNT]>,
        next: &[Face; KEY_COUNT],
    ) -> Result<()> {
        let images = next
            .iter()
            .enumerate()
            .filter(|&(index, face)| previous.is_none_or(|previous| previous[index] != *face))
            .map(|(index, face)| {
                self.render_tile(
                    face.glyph,
                    &face.label,
                    &face.detail,
                    face.tone,
                    face.attention > 0,
                    (face.attention > 0).then_some(true),
                )
                .map(|image| (index, image))
            });
        device
            .write_key_images(images)
            .context("send live key images to Stream Deck")
    }

    fn render_tile(
        &self,
        glyph: &str,
        label: &str,
        detail: &str,
        tone: Tone,
        highlighted: bool,
        enabled: Option<bool>,
    ) -> Result<DynamicImage> {
        let (ink, fill) = tone.colors();
        let rim = if highlighted { ink } else { 0x373c47 };
        let outline = art::outline(glyph).context("missing Stream Deck key glyph")?;
        let indicator = enabled
            .map(|enabled| {
                let color = if enabled { ink } else { 0x697281 };
                format!(r##"<circle cx="62" cy="10" r="2.5" fill="#{color:06x}"/>"##)
            })
            .unwrap_or_default();
        let label = svg_text(label);
        let detail = svg_text(detail);
        let label_y = if detail.is_empty() { 58 } else { 49 };
        let svg = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="72" height="72" viewBox="0 0 72 72">
              <rect width="72" height="72" fill="#101216"/>
              <rect x="1" y="1" width="70" height="70" rx="7" fill="#{fill:06x}" stroke="#{rim:06x}" stroke-width="1.5"/>
              {indicator}
              <path transform="translate(23 13) scale(1.0833)" d="{outline}" fill="none" stroke="#{ink:06x}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>
              <text x="36" y="{label_y}" text-anchor="middle" font-family="Segoe UI, sans-serif" font-size="10" font-weight="600" fill="#f3f4f6">{label}</text>
              <text x="36" y="61" text-anchor="middle" font-family="Segoe UI, sans-serif" font-size="8" fill="#{ink:06x}">{detail}</text>
            </svg>"##
        );
        let tree =
            usvg::Tree::from_str(&svg, &self.options).context("parse Stream Deck key art")?;
        let mut pixels = tiny_skia::Pixmap::new(KEY_IMAGE_SIZE, KEY_IMAGE_SIZE)
            .context("allocate Stream Deck key image")?;
        resvg::render(
            &tree,
            tiny_skia::Transform::identity(),
            &mut pixels.as_mut(),
        );
        let image = RgbaImage::from_raw(KEY_IMAGE_SIZE, KEY_IMAGE_SIZE, pixels.take())
            .ok_or_else(|| anyhow!("invalid Stream Deck key image buffer"))?;
        Ok(DynamicImage::ImageRgba8(image))
    }
}

fn svg_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn changes<'a>(
    previous: Option<&'a Frame>,
    next: &'a Frame,
) -> impl Iterator<Item = (usize, KeyFace)> + 'a {
    next.keys
        .into_iter()
        .enumerate()
        .filter(move |(index, face)| previous.is_none_or(|previous| previous.keys[*index] != *face))
}

#[cfg(test)]
mod tests {
    use super::*;
    use corvid_deck::{Deck, KEY_COUNT};

    #[test]
    fn writes_a_complete_frame_then_only_changed_keys() {
        let mut deck = Deck::default();
        let first = deck.frame();
        assert_eq!(changes(None, &first).count(), KEY_COUNT);
        assert_eq!(changes(Some(&first), &first).count(), 0);

        deck.press(first.capture(0).unwrap()).unwrap();
        let next = deck.frame();
        assert_eq!(
            changes(Some(&first), &next)
                .map(|(index, _)| index)
                .collect::<Vec<_>>(),
            vec![0]
        );
    }

    #[test]
    fn renders_a_full_size_tile_with_label_and_glyph() {
        let renderer = Renderer::new();
        let image = renderer
            .render(Deck::default().frame().keys[0])
            .unwrap()
            .into_rgba8();
        assert_eq!(image.dimensions(), (KEY_IMAGE_SIZE, KEY_IMAGE_SIZE));
        assert!(
            image
                .enumerate_pixels()
                .any(|(_, y, pixel)| y < 40 && pixel[0] > 150)
        );
        assert!(
            image
                .enumerate_pixels()
                .any(|(_, y, pixel)| y > 47 && pixel[0] > 150)
        );
    }
}
