use std::fmt::Write;

use corvid_core::Viewport;
use corvid_core::layout::{Layout, Location};
use corvid_core::navigation::Rect;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;
use vt100::{Cell, Color, Screen};

use super::{navigation, status::clip};

pub(super) struct Composition {
    outer: Viewport,
    pub(super) panes: Vec<(Uuid, Rect)>,
    pub(super) split: bool,
    pub(super) tabs: Vec<(Uuid, Rect, String)>,
    workspace: String,
}

impl Composition {
    pub(super) fn new(outer: Viewport, layout: &Layout, location: Location) -> Self {
        let fit = outer.fit_screen();
        let top = u16::from(fit.rows.get() > 2);
        let usable = Rect {
            x: 0,
            y: top,
            width: fit.cols.get(),
            height: fit.rows.get() - top,
        };
        let mut panes = navigation::panes(layout, location, usable);
        let split = top != 0
            && panes.len() > 1
            && panes
                .iter()
                .all(|(_, pane)| pane.width >= 4 && pane.height >= 3);

        if !split {
            panes = vec![(location.pane, usable)];
        }
        let (workspace, tabs) = top_row(layout, location, fit.cols.get());
        Self {
            outer: fit,
            panes,
            split,
            tabs,
            workspace,
        }
    }

    pub(super) fn body(&self, region: Rect) -> Rect {
        if !self.split {
            return region;
        }
        Rect {
            y: region.y + 1,
            height: region.height - 1,
            width: region.width - u16::from(region.x + region.width < self.outer.cols.get()),
            ..region
        }
    }

    pub(super) fn active_size(&self, location: Location) -> Viewport {
        let pane = self
            .panes
            .iter()
            .find(|(id, _)| *id == location.pane)
            .expect("active pane is present");
        let rect = self.body(pane.1);
        Viewport::new(rect.width, rect.height).expect("pane has a usable area")
    }

    pub(super) fn paint(
        &self,
        location: Location,
        selected: Option<Uuid>,
        titles: &std::collections::HashMap<Uuid, String>,
        mut screen: impl FnMut(Uuid) -> Option<std::sync::Arc<Screen>>,
    ) -> String {
        let mut ansi = String::from("\x1b[?6l\x1b[r\x1b[?7l\x1b[?25l\x1b[0m\x1b[2J");
        if self.panes[0].1.y > 0 {
            let _ = write!(ansi, "\x1b[1;1H\x1b[0;2m{}", self.workspace);
            for (id, rect, title) in &self.tabs {
                let color = if *id == location.tab {
                    "\x1b[0;1;36m"
                } else {
                    "\x1b[0m"
                };
                let _ = write!(ansi, "\x1b[1;{}H{color}{title}", rect.x + 1);
            }
            let hint = "Ctrl+] commands";
            let start = self.outer.cols.get().saturating_sub(hint.len() as u16);
            if start
                >= self
                    .tabs
                    .last()
                    .map_or(0, |(_, rect, _)| rect.x + rect.width + 1)
                && start > self.workspace.width() as u16
            {
                let _ = write!(ansi, "\x1b[1;{}H\x1b[0;2m{hint}", start + 1);
            }
        }
        let mut input = None;
        for (ordinal, &(id, region)) in self.panes.iter().enumerate() {
            let body = self.body(region);
            if self.split {
                let active = id == location.pane;
                let (marker, color) = if Some(id) == selected {
                    ("› ", "\x1b[0;1;7m")
                } else if active {
                    ("● ", "\x1b[0;1;36m")
                } else {
                    ("  ", "\x1b[0;2m")
                };
                let fallback = format!("Pane {}", ordinal + 1);
                let title = titles.get(&id).map_or(fallback.as_str(), String::as_str);
                let label = clip(&format!("{marker}{title}"), usize::from(region.width));
                let _ = write!(
                    ansi,
                    "\x1b[{};{}H{color}{label}",
                    region.y + 1,
                    region.x + 1
                );
                if body.width < region.width {
                    for row in body.y..body.y + body.height {
                        let _ = write!(
                            ansi,
                            "\x1b[{};{}H\x1b[0;2m│",
                            row + 1,
                            body.x + body.width + 1
                        );
                    }
                }
            }
            if let Some(screen) = screen(id) {
                draw(&mut ansi, &screen, body);
                if id == location.pane {
                    let (row, col) = screen.cursor_position();
                    let cursor = if screen.application_cursor() {
                        'h'
                    } else {
                        'l'
                    };
                    let keypad = if screen.application_keypad() {
                        '='
                    } else {
                        '>'
                    };
                    let show = if !screen.hide_cursor() && selected.is_none() {
                        'h'
                    } else {
                        'l'
                    };
                    input = Some(format!(
                        "\x1b[?1{cursor}\x1b{keypad}\x1b[{};{}H\x1b[?25{show}",
                        body.y + row.min(body.height - 1) + 1,
                        body.x + col.min(body.width - 1) + 1
                    ));
                }
            }
        }
        ansi.push_str("\x1b[0m\x1b[?7h\x1b[?2004h\x1b[?1000h\x1b[?1006h");
        if let Some(input) = input {
            ansi.push_str(&input);
        }
        ansi
    }
}

fn top_row(layout: &Layout, location: Location, width: u16) -> (String, Vec<(Uuid, Rect, String)>) {
    let Some(workspace) = layout
        .workspaces
        .iter()
        .find(|ws| ws.id == location.workspace)
    else {
        return (String::new(), Vec::new());
    };
    let prefix = if width >= 26 {
        let label = if width >= 72 {
            if let Some(host) = host_name() {
                format!(
                    "{} / {}",
                    clip(host, usize::from(width / 8)),
                    workspace.label
                )
            } else {
                workspace.label.clone()
            }
        } else {
            workspace.label.clone()
        };
        format!("{}  ", clip(&label, usize::from(width / 3)))
    } else {
        String::new()
    };
    let start = prefix.width() as u16;
    let limit = width - if width >= 48 { 16 } else { 0 };
    let labels: Vec<_> = workspace
        .tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            let label = format!("{} {}", index + 1, tab.label);
            (
                tab.id,
                if tab.id == location.tab {
                    format!("[{label}]")
                } else {
                    label
                },
            )
        })
        .collect();
    let active = labels
        .iter()
        .position(|(id, _)| *id == location.tab)
        .unwrap_or(0);
    let span = usize::from(limit - start);
    let label_width = span.clamp(1, 20);
    let mut first = active;
    let mut available = span.saturating_sub(
        labels
            .get(active)
            .map_or(0, |(_, name)| name.width().min(label_width)),
    );
    while first > 0 {
        let need = labels[first - 1].1.width().min(label_width) + 2;
        if available < need {
            break;
        }
        available -= need;
        first -= 1;
    }
    let mut x = start;
    let mut tabs = Vec::new();
    for (id, label) in labels.into_iter().skip(first) {
        if x >= limit {
            break;
        }
        let label = clip(&label, usize::from(limit - x).min(label_width));
        let width = label.width() as u16;
        tabs.push((
            id,
            Rect {
                x,
                y: 0,
                width,
                height: 1,
            },
            label,
        ));
        x += width + 2;
    }
    (prefix, tabs)
}

fn host_name() -> Option<&'static str> {
    static HOST: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        #[cfg(unix)]
        let name = nix::unistd::gethostname()
            .ok()
            .map(|name| name.to_string_lossy().into_owned());
        #[cfg(windows)]
        let name = std::env::var("COMPUTERNAME").ok();
        name.map(|name| corvid_core::layout::clean_title(&name))
            .filter(|name| !name.is_empty())
    })
    .as_deref()
}

fn draw(ansi: &mut String, screen: &Screen, body: Rect) {
    let (rows, cols) = screen.size();
    let width = cols.get().min(body.width);
    for row in 0..rows.get().min(body.height) {
        let mut style = None;
        let _ = write!(ansi, "\x1b[{};{}H\x1b[0m", body.y + row + 1, body.x + 1);
        for col in 0..width {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let attrs = Style::from(cell);
            if style != Some(attrs) {
                attrs.paint(ansi);
                style = Some(attrs);
            }
            if cell.has_contents() && (!cell.is_wide() || col + 1 < width) {
                ansi.extend(
                    cell.contents()
                        .chars()
                        .filter(|ch| !super::screen::is_bidi_control(*ch)),
                );
            } else {
                ansi.push(' ');
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: Color,
    bg: Color,
    flags: [bool; 5],
}

impl From<&Cell> for Style {
    fn from(cell: &Cell) -> Self {
        Self {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            flags: [
                cell.bold(),
                cell.dim(),
                cell.italic(),
                cell.underline(),
                cell.inverse(),
            ],
        }
    }
}

impl Style {
    fn paint(self, ansi: &mut String) {
        ansi.push_str("\x1b[0");
        for (index, enabled) in self.flags.into_iter().enumerate() {
            if enabled {
                let _ = write!(ansi, ";{}", if index == 4 { 7 } else { index + 1 });
            }
        }
        for (prefix, color) in [(38, self.fg), (48, self.bg)] {
            match color {
                Color::Default => {}
                Color::Idx(index) => {
                    let _ = write!(ansi, ";{prefix};5;{index}");
                }
                Color::Rgb(r, g, b) => {
                    let _ = write!(ansi, ";{prefix};2;{r};{g};{b}");
                }
            }
        }
        ansi.push('m');
    }
}
