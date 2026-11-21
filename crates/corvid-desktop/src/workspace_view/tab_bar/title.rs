use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    App, Bounds, Div, Element, ElementId, ForegroundDependency, GlobalElementId, GlyphRenderMode,
    InspectorElementId, IntoElement, LayoutId, PaintFragment, Pixels, PreparedRasterStyle,
    RasterStyleRequest, RasterizedGlyph, RasterizedGlyphFormat, RenderGlyphParams, ShapedGlyph,
    Stateful, StyledText, TextSystem, Window, hsla_to_rgba, point, prelude::*, px,
};

#[derive(Clone, Default)]
pub(super) struct TitleRenderer(Rc<RefCell<HashMap<RenderGlyphParams, Option<Pixels>>>>);

impl TitleRenderer {
    pub(super) fn button(&self, button: Stateful<Div>, text: String) -> Stateful<Div> {
        // Flex layout lets the title paint its own glyphs. Block layout can
        // consume the text as part of a parent inline layout instead.
        button.flex().aria_label(text.clone()).child(TabTitle {
            text: StyledText::new(text),
            renderer: self.clone(),
        })
    }

    fn color_glyph_center(
        &self,
        text: &TextSystem,
        fragment: &PaintFragment,
        glyph: &ShapedGlyph,
        scale_factor: f32,
    ) -> Option<Pixels> {
        let params = RenderGlyphParams {
            font_id: fragment.font_id,
            glyph_id: glyph.id,
            font_size: fragment.font_size,
            scale_factor,
            subpixel_variant: Default::default(),
            raster_style: PreparedRasterStyle::preblend(RasterStyleRequest {
                font_id: fragment.font_id,
                glyph_id: glyph.id,
                scene_color: hsla_to_rgba(fragment.style.color),
                requested_mode: GlyphRenderMode::Color,
                foreground_dependency: ForegroundDependency::Full,
            }),
        };
        let mut centers = self.0.borrow_mut();
        if let Some(center) = centers.get(&params) {
            return *center;
        }
        if centers.len() >= 256 {
            centers.clear();
        }

        let center = text
            .rasterize_glyph(&params)
            .ok()
            .and_then(|raster| color_ink_center(&raster))
            .map(|center| px(center / scale_factor));
        centers.insert(params, center);
        center
    }
}

fn color_ink_center(raster: &RasterizedGlyph) -> Option<f32> {
    // The rasterizer may pad the image. Use the alpha channel to locate the
    // visible artwork, measuring the center between its top and bottom edges.
    if raster.format != RasterizedGlyphFormat::BgraColor || raster.pixels.is_empty() {
        return None;
    }
    let row_width = raster.size.width.0 as usize * 4;
    let mut rows = raster.pixels.chunks_exact(row_width);
    let has_ink = |row: &[u8]| row.iter().skip(3).step_by(4).any(|alpha| *alpha != 0);
    let top = rows.clone().position(has_ink)?;
    let bottom = rows.rposition(has_ink)? + 1;
    Some(raster.bounds.origin.y.0 as f32 + (top + bottom) as f32 / 2.)
}

struct TabTitle {
    text: StyledText,
    renderer: TitleRenderer,
}

impl IntoElement for TabTitle {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TabTitle {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.text.request_layout(id, inspector, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text.prepaint(id, inspector, bounds, state, window, cx);
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.text.layout();
        let Some(document) = layout.line_layout_for_index(0) else {
            return;
        };
        let line = &document.layout;
        if !line
            .paint_fragments
            .iter()
            .any(|fragment| fragment.glyphs.iter().any(|glyph| glyph.is_emoji))
        {
            self.text
                .paint(id, inspector, bounds, state, prepaint, window, cx);
            return;
        }

        // Retain the shaped visual order and horizontal positions. Center
        // color artwork within the title line while text follows its baseline.
        let bounds = layout.bounds();
        let line_height = layout.line_height();
        let baseline_offset = (line_height + line.ascent - line.descent) / 2.;
        let text_system = cx.text_system().clone();
        let scale_factor = window.scale_factor();
        window.paint_layer(bounds, |window| {
            for (row, visual) in line.visual_lines.iter().enumerate() {
                let top = bounds.origin.y + line_height * row;
                for fragment in &line.paint_fragments[visual.paint_fragment_range.clone()] {
                    for glyph in &*fragment.glyphs {
                        let x = bounds.origin.x + visual.offset + glyph.position.x;
                        let y = top + baseline_offset + glyph.position.y;
                        let color = fragment.style.color;
                        if glyph.is_emoji {
                            let centered_y = self
                                .renderer
                                .color_glyph_center(&text_system, fragment, glyph, scale_factor)
                                .map_or(y, |center| top + line_height / 2. - center);
                            let _ = window.paint_emoji_with_color(
                                point(x, centered_y),
                                fragment.font_id,
                                glyph.id,
                                fragment.font_size,
                                color,
                            );
                        } else {
                            let _ = window.paint_glyph(
                                point(x, y),
                                fragment.font_id,
                                glyph.id,
                                fragment.font_size,
                                color,
                            );
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn measures_visible_emoji_without_transparent_padding() {
        use gpui::{DevicePixels, size};

        use super::*;

        let size = size(DevicePixels(2), DevicePixels(5));
        let mut raster = RasterizedGlyph {
            bounds: Bounds::new(point(DevicePixels(0), DevicePixels(-7)), size),
            size,
            format: RasterizedGlyphFormat::BgraColor,
            pixels: vec![0; 40],
        };
        assert_eq!(color_ink_center(&raster), None);

        // Ink occupies the second and third rows, with asymmetric padding.
        raster.pixels[11] = 255;
        raster.pixels[23] = 100;
        assert_eq!(color_ink_center(&raster), Some(-5.));
    }
}
