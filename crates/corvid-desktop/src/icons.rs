use std::borrow::Cow;

use corvid_deck::art::outline_svg;
use gpui::{AssetSource, Result, SharedString, Styled, Svg, px, rgb, svg};

pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(outline_svg(path).map(|image| Cow::Owned(image.into_bytes())))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

pub fn icon(path: &'static str, size: f32, color: u32) -> Svg {
    svg().path(path).size(px(size)).text_color(rgb(color))
}
