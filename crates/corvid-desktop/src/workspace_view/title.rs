use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    App, Bounds, ContentMask, Display, Div, Element, ElementId, ForegroundDependency,
    GlobalElementId, GlyphRenderMode, InspectorElementId, IntoElement, LayoutId, PaintFragment,
    Pixels, PreparedRasterStyle, RasterStyleRequest, RasterizedGlyph, RasterizedGlyphFormat,
    RenderGlyphParams, ShapedGlyph, SharedString, Stateful, Style, StyledText, TextSystem, Window,
    div, hsla_to_rgba, point, prelude::*, px, rems,
};

const PAUSE_SECONDS: f32 = 1.;
const SPEED_REMS_PER_SECOND: f32 = 2.;

#[derive(Clone, Default)]
pub(super) struct TitleRenderer(Rc<RefCell<HashMap<RenderGlyphParams, Option<Pixels>>>>);

impl TitleRenderer {
    pub(super) fn button(
        &self,
        button: Stateful<Div>,
        label: String,
        suffix: Option<&'static str>,
    ) -> Stateful<Div> {
        let label = SharedString::from(label);
        let accessible_name = suffix.map_or_else(
            || label.clone(),
            |suffix| SharedString::from(format!("{label} {suffix}")),
        );
        // Keep the title in flex layout so it paints its own glyphs.
        button
            .flex()
            .items_center()
            .gap(rems(0.25))
            .aria_label(accessible_name)
            .child(Title {
                text: StyledText::new(label.clone()),
                label,
                renderer: self.clone(),
            })
            .children(suffix.map(|suffix| div().flex_none().child(suffix)))
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

struct Title {
    text: StyledText,
    label: SharedString,
    renderer: TitleRenderer,
}

struct TitleMotion {
    label: SharedString,
    started: Instant,
}

fn title_offset(distance: Pixels, speed: Pixels, elapsed: f32) -> Pixels {
    let travel = distance / speed;
    let leg = PAUSE_SECONDS + travel;
    let phase = elapsed % (2. * leg);
    let progress = (phase % leg - PAUSE_SECONDS).clamp(0., travel);
    if phase < leg {
        progress * speed
    } else {
        distance - progress * speed
    }
}

impl IntoElement for Title {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Title {
    type RequestLayoutState = LayoutId;
    type PrepaintState = bool;

    fn id(&self) -> Option<ElementId> {
        Some("title".into())
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
    ) -> (LayoutId, LayoutId) {
        let (text, ()) = self.text.request_layout(id, inspector, window, cx);
        // The viewport can shrink within the button while the text keeps its intrinsic width.
        let mut style = Style {
            display: Display::Flex,
            ..Default::default()
        };
        style.min_size.width = px(0.).into();
        (window.request_layout(style, [text], cx), text)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        text: &mut LayoutId,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let width = self
            .text
            .layout()
            .line_layout_for_index(0)
            .map_or(px(0.), |line| line.width());
        let distance = width - bounds.size.width;
        let visible = bounds.intersect(&window.content_mask().bounds);
        let scale = window.scale_factor();
        let overflows = distance * scale >= px(0.5);
        let animate = overflows && visible.size.width > px(0.) && visible.size.height > px(0.);
        let offset = window.with_element_state(
            id.expect("title has an element id"),
            |motion: Option<Option<TitleMotion>>, window| {
                if !animate {
                    return (px(0.), None);
                }
                let now = Instant::now();
                let motion = motion
                    .flatten()
                    .filter(|motion| motion.label == self.label)
                    .unwrap_or_else(|| TitleMotion {
                        label: self.label.clone(),
                        started: now,
                    });
                let speed = window.rem_size() * SPEED_REMS_PER_SECOND;
                (
                    title_offset(
                        distance,
                        speed,
                        now.duration_since(motion.started).as_secs_f32(),
                    ),
                    Some(motion),
                )
            },
        );

        window.with_element_offset(point(-offset, px(0.)), |window| {
            let text_bounds = window.layout_bounds(*text);
            self.text
                .prepaint(id, inspector, text_bounds, &mut (), window, cx);
        });
        animate
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut LayoutId,
        animate: &mut bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        if *animate {
            window.request_animation_frame();
        }
        let mask = ContentMask {
            bounds,
            ..Default::default()
        };
        window.with_content_mask(Some(mask), |window| {
            let layout = self.text.layout();
            let Some(document) = layout.line_layout_for_index(0) else {
                return;
            };
            let line = &document.layout;
            if line
                .paint_fragments
                .iter()
                .any(|fragment| fragment.glyphs.iter().any(|glyph| glyph.is_emoji))
            {
                self.paint_with_centered_emoji(line, bounds, window, cx);
            } else {
                self.text
                    .paint(id, inspector, bounds, &mut (), &mut (), window, cx);
            }
        });
    }
}

impl Title {
    fn paint_with_centered_emoji(
        &self,
        line: &gpui::LineLayout,
        viewport: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.text.layout();
        // Retain the shaped visual order and horizontal positions. Center
        // color artwork within the title line while text follows its baseline.
        let bounds = layout.bounds();
        let line_height = layout.line_height();
        let baseline_offset = (line_height + line.ascent - line.descent) / 2.;
        let text_system = cx.text_system().clone();
        let scale_factor = window.scale_factor();
        window.paint_layer(viewport, |window| {
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
    fn overflowing_title_pauses_at_both_ends_and_returns() {
        use super::{px, title_offset};

        let distance = px(64.);
        let speed = px(32.);
        for (seconds, offset) in [
            (0., 0.),
            (0.9, 0.),
            (2., 32.),
            (3., 64.),
            (3.9, 64.),
            (5., 32.),
            (6., 0.),
        ] {
            assert_eq!(title_offset(distance, speed, seconds), px(offset));
        }
    }

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
