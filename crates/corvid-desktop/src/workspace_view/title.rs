use std::time::Instant;

use gpui::{
    App, Bounds, ContentMask, Display, Div, Element, ElementId, GlobalElementId,
    InspectorElementId, LayoutId, LineLayout, Pixels, SharedString, Stateful, Style, StyledText,
    TextRun, TextStyle, Window, div, point, prelude::*, px, rems,
};
use icu_properties::{CodePointSetData, props::EmojiPresentation};
use unicode_segmentation::UnicodeSegmentation;

const PAUSE_SECONDS: f32 = 1.;
const SPEED_REMS_PER_SECOND: f32 = 2.;

pub(super) fn button(
    button: Stateful<Div>,
    label: String,
    suffix: Option<&'static str>,
) -> Stateful<Div> {
    let label = SharedString::from(label);
    let accessible_name = suffix.map_or_else(
        || label.clone(),
        |suffix| SharedString::from(format!("{label} {suffix}")),
    );
    // Flex layout keeps the text in the title's clipped viewport.
    button
        .flex()
        .items_center()
        .gap(rems(0.25))
        .aria_label(accessible_name)
        .child(Title {
            text: StyledText::new(label.clone()),
            label,
        })
        .children(suffix.map(|suffix| div().flex_none().child(suffix)))
}

// GPUI 0.2.2 removes explicitly selected Linux fonts that lack a Latin 'm',
// including Noto Color Emoji. Leave those fonts available through system fallback.
pub(super) fn emoji_presentation_runs(label: &str, style: &TextStyle) -> Option<Vec<TextRun>> {
    if cfg!(target_os = "linux") || label.is_ascii() {
        return None;
    }

    let emoji_family = if cfg!(target_os = "macos") {
        "Apple Color Emoji"
    } else {
        "Segoe UI Emoji"
    };

    let emoji_defaults = CodePointSetData::new::<EmojiPresentation>();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut selects_emoji_font = false;
    for grapheme in label.graphemes(true) {
        let mut run = style.to_run(grapheme.len());
        let default_emoji = grapheme
            .chars()
            .next()
            .is_some_and(|ch| emoji_defaults.contains(ch));
        if !grapheme.contains('\u{fe0e}') && (default_emoji || grapheme.contains('\u{fe0f}')) {
            run.font.family = emoji_family.into();
            run.font.fallbacks = None;
            selects_emoji_font = true;
        }

        if let Some(previous) = runs.last_mut()
            && previous.font == run.font
        {
            previous.len += run.len;
        } else {
            runs.push(run);
        }
    }
    selects_emoji_font.then_some(runs)
}

struct Title {
    text: StyledText,
    label: SharedString,
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
        if let Some(runs) = emoji_presentation_runs(&self.label, &window.text_style()) {
            self.text = StyledText::new(self.label.clone()).with_runs(runs);
        }
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
        let mut text_bounds = window.layout_bounds(*text);
        self.text
            .prepaint(id, inspector, text_bounds, &mut (), window, cx);
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

        if offset > px(0.) {
            text_bounds.origin.x -= offset;
            self.text
                .prepaint(id, inspector, text_bounds, &mut (), window, cx);
        }
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
        let mask = ContentMask { bounds };
        window.with_content_mask(Some(mask), |window| {
            let layout = self.text.layout();
            let Some(wrapped) = layout.line_layout_for_index(0) else {
                return;
            };
            let line = &wrapped.unwrapped_layout;
            if line
                .runs
                .iter()
                .any(|run| run.glyphs.iter().any(|glyph| glyph.is_emoji))
                && !self.label.contains('\n')
            {
                self.paint_with_centered_emoji(line, window, cx);
            } else {
                self.text
                    .paint(id, inspector, bounds, &mut (), &mut (), window, cx);
            }
        });
    }
}

impl Title {
    fn paint_with_centered_emoji(&self, line: &LineLayout, window: &mut Window, cx: &mut App) {
        let layout = self.text.layout();
        let bounds = layout.bounds();
        let line_height = layout.line_height();
        let middle = bounds.origin.y + line_height / 2.;
        let baseline = middle + (line.ascent - line.descent) / 2.;
        let text = cx.text_system();
        let color = window.text_style().color;

        window.paint_layer(|window| {
            for run in &line.runs {
                for glyph in &run.glyphs {
                    let x = bounds.origin.x + glyph.position.x;
                    if glyph.is_emoji {
                        // The original GPUI exposes character metrics. Keep the native baseline
                        // for sequences or fonts that have no usable single-character bounds.
                        let y = self
                            .emoji_baseline(line, run, glyph, text, middle)
                            .unwrap_or(baseline);
                        let _ =
                            window.paint_emoji(point(x, y), run.font_id, glyph.id, line.font_size);
                    } else {
                        let _ = window.paint_glyph(
                            point(x, baseline),
                            run.font_id,
                            glyph.id,
                            line.font_size,
                            color,
                        );
                    }
                }
            }
        });
    }

    fn emoji_baseline(
        &self,
        line: &LineLayout,
        run: &gpui::ShapedRun,
        glyph: &gpui::ShapedGlyph,
        text: &gpui::TextSystem,
        middle: Pixels,
    ) -> Option<Pixels> {
        let grapheme = self.label.get(glyph.index..)?.graphemes(true).next()?;
        let mut characters = grapheme.chars();
        let character = characters.next()?;
        if characters.any(|ch| !matches!(ch, '\u{fe0e}' | '\u{fe0f}')) {
            return None;
        }
        let bounds = text
            .typographic_bounds(run.font_id, line.font_size, character)
            .ok()?;
        (bounds.size.height > px(0.)).then_some(middle + bounds.origin.y + bounds.size.height / 2.)
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
}
