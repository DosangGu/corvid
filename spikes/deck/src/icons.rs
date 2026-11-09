use std::borrow::Cow;

use crate::art::outline;
use gpui::{AssetSource, Result, SharedString, Styled, Svg, px, rgb, svg};

pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let Some(outline) = outline(path) else {
            return Ok(None);
        };
        let image = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><path d="{outline}" fill="none" stroke="white" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>"#
        );
        Ok(Some(Cow::Owned(image.into_bytes())))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

pub fn icon(path: &'static str, size: f32, color: u32) -> Svg {
    svg().path(path).size(px(size)).text_color(rgb(color))
}
