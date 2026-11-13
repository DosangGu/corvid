use std::ops::Range;

use gpui::{
    App, Bounds, FontStyle, FontWeight, Pixels, Point, TextRun, UnderlineStyle, Window, fill, font,
    point, px, rgb, size,
};
use vt100::{Cell, Color, Screen};

use crate::terminal_session::Grid;

pub const BACKGROUND: u32 = 0x12151a;
const FOREGROUND: u32 = 0xe2e8f0;
const FONT_SIZE: Pixels = px(15.);
pub const LINE_HEIGHT: Pixels = px(21.);

#[cfg(test)]
mod performance;

#[derive(Clone, Copy)]
pub struct Metrics {
    column: Pixels,
}

impl Metrics {
    pub fn measure(window: &Window) -> Self {
        let line = window.text_system().shape_line(
            "M".into(),
            FONT_SIZE,
            &[text_run(1, Style::default())],
            None,
        );
        Self {
            column: line.width.max(px(1.)),
        }
    }

    pub fn grid(self, bounds: Bounds<Pixels>) -> Grid {
        Grid::new(
            (f32::from(bounds.size.height) / f32::from(LINE_HEIGHT)) as u16,
            (f32::from(bounds.size.width) / f32::from(self.column)) as u16,
        )
    }

    pub fn cell(self, origin: Point<Pixels>, row: u16, col: u16, count: u16) -> Bounds<Pixels> {
        Bounds {
            origin: origin + point(self.column * f32::from(col), LINE_HEIGHT * f32::from(row)),
            size: size(self.column * f32::from(count), LINE_HEIGHT),
        }
    }

    pub fn position(self, bounds: Bounds<Pixels>, point: Point<Pixels>) -> (u16, u16) {
        let grid = self.grid(bounds);
        let relative = point - bounds.origin;
        let row = ((relative.y / LINE_HEIGHT) as u16).min(grid.rows.get() - 1);
        let col = ((relative.x / self.column) as u16).min(grid.cols.get() - 1);
        (row, col)
    }
}

#[derive(Default)]
pub struct Frame {
    spans: Vec<Span>,
    cursor: Option<(u16, u16)>,
}

struct Span {
    row: u16,
    col: u16,
    width: u16,
    style: Style,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: u32,
    bg: u32,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: FOREGROUND,
            bg: BACKGROUND,
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

impl Style {
    fn from_cell(cell: &Cell) -> Self {
        let mut fg = color(cell.fgcolor(), FOREGROUND);
        let mut bg = color(cell.bgcolor(), BACKGROUND);
        if cell.inverse() {
            std::mem::swap(&mut fg, &mut bg);
        }
        Self {
            fg,
            bg,
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
        }
    }
}

impl Frame {
    pub fn new(screen: &Screen, grid: Grid) -> Self {
        Self::selected(screen, grid, None)
    }

    pub fn selected(screen: &Screen, grid: Grid, selection: Option<&Range<(u16, u16)>>) -> Self {
        let (rows, cols) = screen.size();
        let mut spans: Vec<Span> = Vec::new();
        for row in 0..rows.get().min(grid.rows.get()) {
            for col in 0..cols.get().min(grid.cols.get()) {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut style = Style::from_cell(cell);
                if selection.is_some_and(|selection| selection.contains(&(row, col))) {
                    style.bg = 0x315773;
                    style.fg = FOREGROUND;
                }
                if !cell.has_contents() && style.bg == BACKGROUND {
                    continue;
                }
                let text = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                if text.len() == 1
                    && let Some(previous) = spans.last_mut()
                    && previous.row == row
                    && previous.col + previous.width == col
                    && previous.style == style
                    && previous.text.is_ascii()
                {
                    previous.text.push_str(text);
                    previous.width += 1;
                } else {
                    spans.push(Span {
                        row,
                        col,
                        width: if cell.is_wide() { 2 } else { 1 },
                        style,
                        text: text.to_owned(),
                    });
                }
            }
        }
        let (row, col) = screen.cursor_position();
        let cursor = (selection.is_none()
            && !screen.hide_cursor()
            && screen.scrollback() == 0
            && row < grid.rows.get())
        .then_some((row, col.min(grid.cols.get() - 1)));
        Self { spans, cursor }
    }

    pub fn paint(
        self,
        bounds: Bounds<Pixels>,
        metrics: Metrics,
        active: bool,
        marked: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        // ShapedLine::paint creates a scene layer per span. A terminal can have
        // thousands of one-cell spans; inserting each into GPUI's bounds tree
        // is costly on the UI thread. Batch backgrounds and glyphs in separate layers.
        window.paint_layer(bounds, |window| {
            for span in &self.spans {
                let cell = metrics.cell(bounds.origin, span.row, span.col, span.width);
                if span.style.bg != BACKGROUND {
                    window.paint_quad(fill(cell, rgb(span.style.bg)));
                }
            }
        });
        window.paint_layer(bounds, |window| {
            for span in self.spans {
                let cell = metrics.cell(bounds.origin, span.row, span.col, span.width);
                paint_text(span.text, span.style, cell.origin, window);
            }
        });
        if active && let Some((row, col)) = self.cursor {
            let cell = metrics.cell(bounds.origin, row, col, 1);
            if marked.is_empty() {
                window.paint_quad(fill(
                    Bounds {
                        origin: cell.origin,
                        size: size(px(2.), LINE_HEIGHT),
                    },
                    rgb(0x8ae3c4),
                ));
            } else {
                let style = Style {
                    fg: 0x8ae3c4,
                    bg: 0x20332f,
                    underline: true,
                    ..Style::default()
                };
                let marked = window.text_system().shape_line(
                    marked.clone().into(),
                    FONT_SIZE,
                    &[text_run(marked.len(), style)],
                    None,
                );
                window.paint_quad(fill(
                    Bounds {
                        origin: cell.origin,
                        size: size(marked.width, LINE_HEIGHT),
                    },
                    rgb(style.bg),
                ));
                let _ = marked.paint(cell.origin, LINE_HEIGHT, window, cx);
            }
        }
    }
}

fn paint_text(text: String, style: Style, origin: Point<Pixels>, window: &mut Window) {
    if text.trim().is_empty() {
        return;
    }
    let run = text_run(text.len(), style);
    let line = window
        .text_system()
        .shape_line(text.into(), FONT_SIZE, &[run], None);
    let baseline = origin
        + point(
            px(0.),
            (LINE_HEIGHT - line.ascent - line.descent) / 2. + line.ascent,
        );
    for run in &line.runs {
        for glyph in &run.glyphs {
            let position = baseline + point(glyph.position.x, px(0.));
            if glyph.is_emoji {
                let _ = window.paint_emoji(position, run.font_id, glyph.id, FONT_SIZE);
            } else {
                let _ = window.paint_glyph(
                    position,
                    run.font_id,
                    glyph.id,
                    FONT_SIZE,
                    rgb(style.fg).into(),
                );
            }
        }
    }
    if style.underline {
        let first_x = line
            .runs
            .iter()
            .flat_map(|run| &run.glyphs)
            .next()
            .map_or(px(0.), |glyph| glyph.position.x);
        window.paint_underline(
            baseline + point(first_x, line.descent * 0.618),
            line.width,
            &UnderlineStyle {
                thickness: px(1.),
                color: Some(rgb(style.fg).into()),
                wavy: false,
            },
        );
    }
}

fn text_run(len: usize, style: Style) -> TextRun {
    let mut font = font("Consolas");
    if style.bold {
        font.weight = FontWeight::BOLD;
    }
    if style.italic {
        font.style = FontStyle::Italic;
    }
    TextRun {
        len,
        font,
        color: rgb(style.fg).into(),
        background_color: None,
        underline: style.underline.then_some(UnderlineStyle {
            thickness: px(1.),
            color: None,
            wavy: false,
        }),
        strikethrough: None,
    }
}

fn color(color: Color, default: u32) -> u32 {
    const ANSI: [u32; 16] = [
        0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
        0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
    ];
    let rgb = |r: u8, g: u8, b: u8| u32::from_be_bytes([0, r, g, b]);
    match color {
        Color::Default => default,
        Color::Rgb(r, g, b) => rgb(r, g, b),
        Color::Idx(index @ 0..=15) => ANSI[usize::from(index)],
        Color::Idx(index @ 16..=231) => {
            let level = [0, 95, 135, 175, 215, 255];
            let index = usize::from(index - 16);
            rgb(level[index / 36], level[index / 6 % 6], level[index % 6])
        }
        Color::Idx(index) => {
            let gray = 8 + (index - 232) * 10;
            rgb(gray, gray, gray)
        }
    }
}
