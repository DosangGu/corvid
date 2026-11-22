use super::*;
use gpui::{App, Bounds, TitlebarOptions, WeakEntity, WindowBounds, WindowOptions, px, size};

use crate::appearance::{Change, Scope, TERMINAL_SHORTCUTS};
use crate::terminal_fonts::TerminalFonts;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Connections,
    Appearance,
    Terminal,
    Shortcuts,
    Agents,
    About,
}

pub(super) struct ManagementView {
    owner: WeakEntity<WorkspaceView>,
}

impl ManagementView {
    fn new(owner: Entity<WorkspaceView>, window: &Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&owner, |_, _, cx| cx.notify()).detach();
        cx.observe_release_in(&owner, window, |_, _, window, _| window.remove_window())
            .detach();
        Self {
            owner: owner.downgrade(),
        }
    }
}

impl Render for ManagementView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(cx.global::<Appearance>().rem_size());
        self.owner
            .update(cx, |owner, cx| owner.management_ui(cx))
            .unwrap_or_else(|_| div().id("settings-unavailable"))
    }
}

impl WorkspaceView {
    pub(super) fn open_management(
        &mut self,
        page: Page,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.management_page = page;
        self.dismiss_overlay(window, cx);
        self.refresh_hosts(cx);
        #[cfg(windows)]
        self.refresh_wsl(cx);
        let owner = cx.entity();
        let focus = self.management_focus.clone();
        // GPUI draws a new window before open_window returns. Defer creation so
        // its first render can read the owner after this update has finished.
        cx.defer(move |cx| {
            if owner.read(cx).management_window.is_some_and(|handle| {
                handle
                    .update(cx, |_, window, _| window.activate_window())
                    .is_ok()
            }) {
                return;
            }
            let options = WindowOptions {
                app_id: Some(crate::app_icon::APP_ID.into()),
                titlebar: Some(TitlebarOptions {
                    title: Some("Corvid Settings".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(740.), px(580.)),
                    cx,
                ))),
                window_min_size: Some(size(px(580.), px(420.))),
                ..Default::default()
            };
            let settings_owner = owner.clone();
            let result = cx.open_window(options, move |window, cx| {
                #[cfg(target_os = "linux")]
                if let Err(error) = crate::app_icon::set_window_icons() {
                    eprintln!("Could not set Corvid Settings icon: {error}");
                }
                window.focus(&focus, cx);
                cx.new(|cx| ManagementView::new(settings_owner, window, cx))
            });
            owner.update(cx, |view, cx| {
                match result {
                    Ok(handle) => view.management_window = Some(handle),
                    Err(error) => view.error = Some(format!("Could not open settings: {error}")),
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn management_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if appearance::terminal_shortcut(event, window, cx) {
            return;
        }
        let key = &event.keystroke;
        if key.key == "escape" {
            window.remove_window();
            let terminal = self.terminal_window;
            cx.defer(move |cx| {
                let _ = terminal.update(cx, |view, window, cx| {
                    window.activate_window();
                    view.focus_terminal(window, cx);
                });
            });
            cx.stop_propagation();
            return;
        }
        if key.key == "tab" {
            if key.modifiers.shift {
                window.focus_prev(cx);
            } else {
                window.focus_next(cx);
            }
            cx.stop_propagation();
            return;
        }
        if !self.alias_focus.is_focused(window) || self.management_page != Page::Connections {
            return;
        }
        cx.stop_propagation();
        if self.connecting(self.session.as_ref().map(Session::ui).as_ref()) {
            return;
        }
        if key.key == "enter" {
            self.connect_host(self.alias.clone(), window, cx);
        } else if key.key == "backspace" {
            self.alias.pop();
        } else if key.modifiers.control && key.key == "v" {
            if let Some(value) = cx.read_from_clipboard().and_then(|item| item.text()) {
                let value = value.trim();
                if value.len() <= 253 && value.bytes().all(alias_byte) {
                    self.alias = value.to_owned();
                }
            }
        } else if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform {
            let value = key.key_char.as_deref().unwrap_or(&key.key);
            if value.len() == 1 && self.alias.len() < 253 && value.bytes().all(alias_byte) {
                self.alias.push_str(value);
            }
        }
        cx.notify();
    }

    fn management_ui(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let ui = self.session.as_ref().map(Session::ui);
        let pages = [
            (Page::Connections, "Connections"),
            (Page::Appearance, "Appearance"),
            (Page::Terminal, "Terminal"),
            (Page::Shortcuts, "Shortcuts"),
            (Page::Agents, "Agents"),
            (Page::About, "About & updates"),
        ];
        let content = match self.management_page {
            Page::Connections => self.connections_panel(ui.as_ref(), cx),
            Page::Appearance => appearance_settings(cx),
            Page::Terminal => self.terminal_settings(cx),
            Page::Shortcuts => shortcuts(),
            Page::Agents => settings_page("Agents")
                .child(description("View agent activity and input requests in a separate window. Select an agent to return to its terminal."))
                .child(description("In the Agents window, open Notifications to choose notification types, sound, and pauses."))
                .when(ui.as_ref().is_some_and(|ui| ui.connected), |page| {
                    page.child(button("open-agents-settings", "Open Agents", true).on_click(
                        cx.listener(|view, _, _, _| {
                            if let Some(session) = &view.session {
                                session.open_agent_deck(view.agent_deck.clone());
                            }
                        }),
                    ))
                })
                .when(!ui.as_ref().is_some_and(|ui| ui.connected), |page| {
                    page.child(description("Connect to a terminal to view its agents."))
                })
                .child(description("Questions and approvals are answered in the terminal.")),
            Page::About => settings_page("About & updates")
                .child(self.update_menu(cx)),
        };
        div()
            .id("management")
            .font(crate::desktop_font::with_symbols(".SystemUIFont"))
            .track_focus(&self.management_focus)
            .tab_group()
            .capture_key_down(cx.listener(Self::management_key_down))
            .flex()
            .size_full()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(ui_px(164.))
                    .p(ui_px(12.))
                    .gap(ui_px(6.))
                    .bg(rgb(PANEL))
                    .children(pages.into_iter().map(|(page, label)| {
                        button(label, label, self.management_page == page).on_click(cx.listener(
                            move |view, _, window, cx| {
                                view.management_page = page;
                                window.focus(&view.management_focus, cx);
                                cx.notify();
                            },
                        ))
                    })),
            )
            .child(
                div()
                    .id("management-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(content),
            )
    }

    fn terminal_settings(&self, cx: &mut Context<Self>) -> Div {
        let zoom_label = if self.zoom_pane {
            "☑ Expand the active pane"
        } else {
            "☐ Expand the active pane"
        };
        let zoom = button("zoom-pane-setting", zoom_label, self.zoom_pane).on_click(cx.listener(
            |view, _, _, cx| {
                view.zoom_pane = !view.zoom_pane;
                cx.notify();
            },
        ));
        settings_page("Terminal")
            .child(Self::terminal_clipboard_settings(cx))
            .child(Self::terminal_font_settings(cx))
            .child(zoom)
            .child(description(concat!(
                "Use the command list to expand the active pane or restore the split layout. ",
                "These view settings apply to this window.",
            )))
    }

    fn terminal_clipboard_settings(cx: &mut Context<Self>) -> Div {
        let enabled = cx.global::<TerminalFonts>().allows_clipboard_write();
        let label = if enabled {
            "☑ Allow terminal apps to copy to the clipboard"
        } else {
            "☐ Allow terminal apps to copy to the clipboard"
        };
        div()
            .flex()
            .flex_col()
            .gap(ui_px(6.))
            .child(button("terminal-clipboard-write", label, enabled).on_click(
                cx.listener(move |_, _, window, cx| {
                    match cx.global_mut::<TerminalFonts>().set_clipboard_write(!enabled) {
                        Ok(()) => cx.refresh_windows(),
                        Err(error) => {
                            drop(window.prompt(
                                PromptLevel::Warning,
                                "Could not save the clipboard setting",
                                Some(&format!("{error:#}")),
                                &["OK"],
                                cx,
                            ));
                        }
                    }
                }),
            ))
            .child(description("Programs in the focused terminal pane, including SSH sessions, can copy text to this computer."))
    }

    fn terminal_font_settings(cx: &mut Context<Self>) -> Div {
        let fonts = cx.global::<TerminalFonts>();
        let default = fonts.default_family().clone();
        let selected = fonts.selected().cloned();
        let available = fonts.available().to_vec();
        let use_default = button(
            "terminal-font-default",
            format!("Default ({default})"),
            selected.is_none(),
        )
        .on_click(cx.listener(|_, _, window, cx| set_terminal_font(None, window, cx)));

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(ui_px(8.))
            .child(div().text_size(ui_px(14.)).child("Terminal font"))
            .child(description("Applies to all terminal panes."))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(ui_px(6.))
                    .child(use_default)
                    .children(available.into_iter().map(|family| {
                        button(
                            format!("terminal-font-{family}"),
                            family.clone(),
                            selected.as_ref() == Some(&family),
                        )
                        .on_click(cx.listener(move |_, _, window, cx| {
                            set_terminal_font(Some(family.clone()), window, cx);
                        }))
                    })),
            )
    }
}

fn appearance_settings(cx: &mut Context<WorkspaceView>) -> Div {
    let appearance = cx.global::<Appearance>();
    let app = appearance.value(Scope::App);
    let terminal = appearance.value(Scope::Terminal);
    settings_page("Appearance")
        .child(size_setting(
            "App scale",
            "Scales the workspace, settings, and terminal together.",
            format!("{app}%"),
            Scope::App,
            cx,
        ))
        .child(size_setting(
            "Terminal font size",
            "Changes the text in all terminal panes. App scale also applies.",
            format!("{terminal} px"),
            Scope::Terminal,
            cx,
        ))
        .child(description(TERMINAL_SHORTCUTS))
}

fn size_setting(
    label: &'static str,
    help: &'static str,
    value: String,
    scope: Scope,
    cx: &mut Context<WorkspaceView>,
) -> Div {
    let appearance = cx.global::<Appearance>();
    let controls = [
        ("Decrease", Change::Decrease),
        ("Reset", Change::Reset),
        ("Increase", Change::Increase),
    ]
    .map(|(text, change)| {
        let id = format!("{label}-{text}");
        if appearance.target(scope, change) == appearance.value(scope) {
            div()
                .flex_none()
                .px(ui_px(10.))
                .py(ui_px(5.))
                .text_size(ui_px(13.))
                .text_color(rgb(MUTED))
                .opacity(0.5)
                .child(text)
                .into_any_element()
        } else {
            button(id, text, false)
                .on_click(move |_, window, cx| appearance::adjust(scope, change, window, cx))
                .into_any_element()
        }
    });
    div()
        .flex()
        .flex_col()
        .gap(ui_px(6.))
        .child(div().text_size(ui_px(14.)).child(label))
        .child(description(help))
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(ui_px(6.))
                .child(div().min_w(ui_px(46.)).text_size(ui_px(14.)).child(value))
                .children(controls),
        )
}

fn set_terminal_font(family: Option<SharedString>, window: &mut Window, cx: &mut App) {
    match cx.global_mut::<TerminalFonts>().select(family) {
        Ok(()) => cx.refresh_windows(),
        Err(error) => {
            drop(window.prompt(
                PromptLevel::Warning,
                "Could not save the terminal font",
                Some(&format!("{error:#}")),
                &["OK"],
                cx,
            ));
        }
    }
}

fn settings_page(title: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .gap(ui_px(16.))
        .p(ui_px(24.))
        .child(div().text_size(ui_px(20.)).child(title))
}

fn description(text: &'static str) -> Div {
    div()
        .text_size(ui_px(13.))
        .text_color(rgb(MUTED))
        .child(text)
}

fn shortcuts() -> Div {
    use corvid_core::navigation::WorkspaceCommand;
    let keys = WorkspaceCommand::ALL
        .into_iter()
        .map(|command| (format!("Ctrl+] → {}", command.key()), command.label()))
        .chain([
            ("Ctrl+] → Arrow".into(), "Focus a neighboring pane"),
            ("Ctrl+Shift+C / Ctrl+Shift+V".into(), "Copy / paste"),
            (
                TERMINAL_SHORTCUTS.into(),
                "Adjust / reset terminal font size",
            ),
            ("Esc".into(), "Close a menu or settings"),
        ]);
    settings_page("Shortcuts").children(keys.into_iter().map(|(key, label)| {
        div()
            .flex()
            .justify_between()
            .gap(ui_px(12.))
            .text_size(ui_px(13.))
            .child(label)
            .child(div().text_color(rgb(MUTED)).child(key))
    }))
}
