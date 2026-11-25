use std::fmt::Write;
use std::time::Instant;

use super::screen::{CellWidth, ScreenSnapshot};
use corvid_core::Viewport;
use corvid_core::layout::{Layout, Location};
use corvid_core::navigation::Rect;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::title_motion::{TitleMotion, window};
use super::{navigation, status::clip};
use crate::metadata::DisplayLayout;

pub(super) struct Composition {
    outer: Viewport,
    pub(super) panes: Vec<(Uuid, Rect)>,
    pub(super) split: bool,
    pub(super) tabs: Vec<TabLabel>,
    workspace: Option<WorkspaceLabel>,
    pub(super) tab_mode: bool,
}

pub(super) struct TabLabel {
    pub(super) id: Uuid,
    pub(super) rect: Rect,
    number: String,
    title: String,
    active: bool,
    selected: bool,
}

struct WorkspaceLabel {
    id: Uuid,
    host: String,
    title: String,
    width: usize,
}

#[derive(Default)]
pub(super) struct HeaderMotion {
    workspace: TitleMotion,
    tab: TitleMotion,
}

impl TabLabel {
    fn decoration_width(&self) -> usize {
        self.number.len() + usize::from(self.active) * 2 + 1
    }

    fn title_only(&self) -> bool {
        self.selected
            && !self.title.is_empty()
            && usize::from(self.rect.width) <= self.decoration_width()
    }

    fn title_width(&self) -> usize {
        let width = usize::from(self.rect.width);
        if self.title_only() {
            return width;
        }
        width.saturating_sub(self.decoration_width())
    }

    fn paint(&self, ansi: &mut String, motion: Option<&TitleMotion>) {
        let (open, close) = if self.active { ("[", "]") } else { ("", "") };
        let color = if self.selected {
            "\x1b[0;1;7m"
        } else if self.active {
            "\x1b[0;1;36m"
        } else {
            "\x1b[0m"
        };
        let title = motion.map_or_else(
            || window(&self.title, self.title_width(), 0),
            |motion| motion.visible().to_owned(),
        );
        let label = if self.title_only() {
            title
        } else if self.title_width() == 0 || self.title.is_empty() {
            format!("{open}{}{close}", self.number)
        } else if self.active {
            let padding = self.title_width().saturating_sub(title.width());
            format!("{open}{} {title}{:padding$}{close}", self.number, "")
        } else {
            format!("{} {title}", self.number)
        };
        let _ = write!(
            ansi,
            "\x1b[1;{}H{color}{}",
            self.rect.x + 1,
            window(&label, usize::from(self.rect.width), 0),
        );
    }
}

impl Composition {
    pub(super) fn new(
        outer: Viewport,
        layout: &Layout,
        location: Location,
        selected_tab: Option<Uuid>,
        display: Option<&DisplayLayout>,
    ) -> Self {
        let outer = outer.fit_screen();
        let (panes, split) = Self::pane_layout(outer, layout, location);
        let (workspace, tabs) = top_row(layout, location, outer.cols.get(), selected_tab, display);
        Self {
            outer,
            panes,
            split,
            tabs,
            workspace,
            tab_mode: selected_tab.is_some(),
        }
    }

    pub(super) fn show_tab(&mut self, layout: &Layout, location: Location) {
        (self.panes, self.split) = Self::pane_layout(self.outer, layout, location);
    }

    fn pane_layout(
        outer: Viewport,
        layout: &Layout,
        location: Location,
    ) -> (Vec<(Uuid, Rect)>, bool) {
        let top = u16::from(outer.rows.get() > 2);
        let usable = Rect {
            x: 0,
            y: top,
            width: outer.cols.get(),
            height: outer.rows.get() - top,
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
        (panes, split)
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

    fn animated_tab(&self) -> Option<&TabLabel> {
        self.tabs
            .iter()
            .find(|tab| tab.selected)
            .or_else(|| self.tabs.iter().find(|tab| tab.active))
    }

    pub(super) fn update_header(&self, motion: &mut HeaderMotion, now: Instant) -> bool {
        if self.panes[0].1.y == 0 && !self.tab_mode {
            return false;
        }
        let workspace = self
            .workspace
            .as_ref()
            .map(|name| (name.id, name.title.as_str(), name.width));
        let tab = self
            .animated_tab()
            .map(|tab| (tab.id, tab.title.as_str(), tab.title_width()));
        let workspace_changed = motion.workspace.update(workspace, now);
        motion.tab.update(tab, now) || workspace_changed
    }

    fn paint_header(&self, ansi: &mut String, motion: &HeaderMotion) {
        if self.panes[0].1.y == 0 && !self.tab_mode {
            return;
        }
        ansi.push_str("\x1b[1;1H\x1b[0m\x1b[2K");
        let workspace_width = if let Some(label) = &self.workspace {
            let _ = write!(
                ansi,
                "\x1b[0;2m{}{}",
                label.host,
                motion.workspace.visible()
            );
            label.host.width() as u16 + label.width as u16
        } else {
            0
        };
        let animated = self.animated_tab().map(|tab| tab.id);
        for tab in &self.tabs {
            tab.paint(ansi, (Some(tab.id) == animated).then_some(&motion.tab));
        }
        if self.tab_mode {
            return;
        }
        let hint = "Ctrl+] commands";
        let start = self.outer.cols.get().saturating_sub(hint.len() as u16);
        if start
            >= self
                .tabs
                .last()
                .map_or(0, |tab| tab.rect.x + tab.rect.width + 1)
            && start > workspace_width
        {
            let _ = write!(ansi, "\x1b[1;{}H\x1b[0;2m{hint}", start + 1);
        }
    }

    pub(super) fn repaint_header(&self, motion: &HeaderMotion) -> String {
        // Preserve cursor state and keep a title in the rightmost column from wrapping.
        let mut ansi = String::from("\x1b7\x1b[?7l");
        self.paint_header(&mut ansi, motion);
        ansi.push_str("\x1b[0m\x1b[?7h\x1b8");
        ansi
    }

    pub(super) fn paint(
        &self,
        location: Location,
        selected: Option<Uuid>,
        display: &DisplayLayout,
        header: &HeaderMotion,
        mut screen: impl FnMut(Uuid) -> Option<std::sync::Arc<ScreenSnapshot>>,
    ) -> String {
        let mut ansi = String::from("\x1b[?6l\x1b[r\x1b[?7l\x1b[?25l\x1b[0m\x1b[2J");
        if self.panes[0].1.y != 0 {
            self.paint_header(&mut ansi, header);
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
                let title = display
                    .titles
                    .get(&id)
                    .map_or(fallback.as_str(), String::as_str);
                let status = pane_status(display, id);
                let label = status.map_or_else(
                    || format!("{marker}{title}"),
                    |status| format!("{marker}{status} · {title}"),
                );
                let label = clip(&label, usize::from(region.width));
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
                    let show = if !screen.hide_cursor() && selected.is_none() && !self.tab_mode {
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
        if self.panes[0].1.y == 0 && self.tab_mode {
            self.paint_header(&mut ansi, header);
        }
        ansi.push_str("\x1b[0m\x1b[?7h\x1b[?2004h\x1b[?1000h\x1b[?1006h");
        if let Some(input) = input {
            ansi.push_str(&input);
        }
        ansi
    }
}

fn top_row(
    layout: &Layout,
    location: Location,
    width: u16,
    selected_tab: Option<Uuid>,
    display: Option<&DisplayLayout>,
) -> (Option<WorkspaceLabel>, Vec<TabLabel>) {
    let Some(workspace) = layout
        .workspaces
        .iter()
        .find(|ws| ws.id == location.workspace)
    else {
        return (None, Vec::new());
    };
    let limit = width
        - if width >= 48 && selected_tab.is_none() {
            16
        } else {
            0
        };
    let workspace_label = (width >= 16).then(|| {
        let budget = usize::from(width / 3);
        let host = if width >= 72 {
            host_name()
                .map(|name| format!("{} / ", clip(name, usize::from(width / 8))))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let title = corvid_core::layout::clean_title(&workspace.label);
        let width = title.width().min(budget.saturating_sub(host.width()));
        WorkspaceLabel {
            id: workspace.id,
            host,
            title,
            width,
        }
    });
    let start = workspace_label
        .as_ref()
        .map_or(0, |label| (label.host.width() + label.width + 2) as u16);
    let label_width = usize::from(limit - start).clamp(1, 20);
    let mut tabs: Vec<_> = workspace
        .tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            let number = (index + 1).to_string();
            let mut title = corvid_core::layout::clean_title(&tab.label);
            let active = tab.id == location.tab;
            if let Some(display) = display {
                let panes = || {
                    display
                        .layout
                        .locations()
                        .filter(|location| location.tab == tab.id)
                };
                let focused = active || Some(tab.id) == selected_tab;
                let pending =
                    panes().find(|location| display.pending_input_panes.contains(&location.pane));
                let status = if let Some(pending) = pending {
                    if focused {
                        pane_status(display, pending.pane)
                    } else {
                        Some("Needs input".to_owned())
                    }
                } else {
                    panes()
                        .filter_map(|location| display.program_statuses.get(&location.pane))
                        .max_by_key(|status| status.summary_priority())
                        .map(|status| {
                            if focused {
                                status.detail()
                            } else {
                                status.summary()
                            }
                        })
                };
                if let Some(status) = status {
                    title = format!("{status} · {title}");
                }
            }
            let size = number.len() + usize::from(active) * 2 + 1 + title.width();
            TabLabel {
                id: tab.id,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: size.min(label_width) as u16,
                    height: 1,
                },
                number,
                title,
                active,
                selected: Some(tab.id) == selected_tab,
            }
        })
        .collect();
    let anchor = tabs
        .iter()
        .position(|tab| tab.selected)
        .or_else(|| tabs.iter().position(|tab| tab.active))
        .unwrap_or(0);
    let mut first = anchor;
    let mut remaining =
        (limit - start).saturating_sub(tabs.get(anchor).map_or(0, |tab| tab.rect.width));
    while first > 0 {
        let required = tabs[first - 1].rect.width + 2;
        if remaining < required {
            break;
        }
        remaining -= required;
        first -= 1;
    }
    let mut x = start;
    tabs.drain(..first);
    tabs.retain_mut(|tab| {
        if x >= limit {
            return false;
        }
        tab.rect.x = x;
        tab.rect.width = tab.rect.width.min(limit - x);
        x += tab.rect.width + 2;
        true
    });
    (workspace_label, tabs)
}

pub(super) fn pane_status(display: &DisplayLayout, pane: Uuid) -> Option<String> {
    let status = display.program_statuses.get(&pane);
    if display.pending_input_panes.contains(&pane) {
        return Some(status.map_or_else(
            || "Needs input".to_owned(),
            |status| format!("Needs input · {}", status.detail()),
        ));
    }
    status.map(|status| status.detail())
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

fn draw(ansi: &mut String, screen: &ScreenSnapshot, body: Rect) {
    let (rows, cols) = screen.size();
    let width = cols.get().min(body.width);
    for row in 0..rows.get().min(body.height) {
        let mut style = None;
        let _ = write!(ansi, "\x1b[{};{}H\x1b[0m", body.y + row + 1, body.x + 1);
        for col in 0..width {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.width == CellWidth::Continuation {
                continue;
            }
            let attrs = cell.style;
            if style != Some(attrs) {
                attrs.paint(ansi);
                style = Some(attrs);
            }
            if cell.has_contents() && (cell.width == CellWidth::Single || col + 1 < width) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn header_motion_preserves_the_tab_hitboxes_and_terminal_cursor() {
        let outer = Viewport::new(44, 12).unwrap();
        let mut layout = Layout::empty_terminal();
        let location = layout.primary().unwrap();
        layout.workspaces[0].label = "workspace-review-WEST".into();
        layout.workspaces[0].tabs[0].label = "shared-prefix-한글-TAIL".into();
        layout.add_tab(location.workspace, "Run".into()).unwrap();
        let view = Composition::new(outer, &layout, location, None, None);
        let start = Instant::now();
        let mut motion = HeaderMotion::default();
        assert!(view.update_header(&mut motion, start));
        let mut terminal = vt100::Parser::new(outer.rows, outer.cols, 0);
        let mut frame = String::from("\x1b[?7l");
        view.paint_header(&mut frame, &motion);
        frame.push_str("\x1b[?7h\x1b[3;4H\x1b[31m");
        terminal.process(frame.as_bytes());
        let hitboxes = view
            .tabs
            .iter()
            .map(|tab| (tab.id, tab.rect))
            .collect::<Vec<_>>();
        let before = terminal.screen().contents();
        assert!(before.contains("[1 shared-prefix"));

        assert!(view.update_header(&mut motion, start + Duration::from_millis(2000)));
        let patch = view.repaint_header(&motion);
        assert!(!patch.contains("\x1b[2J"));
        terminal.process(patch.as_bytes());
        assert!(terminal.screen().contents().contains("TAIL]"));
        assert_eq!(terminal.screen().cursor_position(), (2, 3));
        assert_eq!(terminal.screen().fgcolor(), vt100::Color::Idx(1));
        assert_eq!(
            hitboxes,
            view.tabs
                .iter()
                .map(|tab| (tab.id, tab.rect))
                .collect::<Vec<_>>(),
        );
    }
}
