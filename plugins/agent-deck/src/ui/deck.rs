use super::*;
use gpui::{linear_color_stop, linear_gradient, svg};
use std::sync::Arc;

pub(super) fn column() -> Div {
    div().flex().flex_col().gap(px(8.))
}
pub(super) fn row() -> Div {
    div().flex().items_center().gap(px(8.))
}
impl Colors {
    pub(super) fn muted(self, text: impl Into<SharedString>) -> Div {
        div()
            .text_color(self.muted)
            .text_size(px(12.))
            .child(text.into())
    }
    pub(super) fn button(
        self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        enabled: bool,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .flex_shrink_0()
            .px(px(8.))
            .py(px(5.))
            .rounded_md()
            .border_1()
            .border_color(self.border)
            .bg(self.panel)
            .text_color(if enabled { self.text } else { self.muted })
            .when(enabled, |button| {
                button.cursor_pointer().hover(|style| style.bg(self.hover))
            })
            .child(label.into())
    }
}

impl DeckView {
    pub(super) fn focus_action(
        &self,
        snapshot: &Arc<Snapshot>,
        slot: Option<&PresentedSlot>,
        run: Option<&PresentedRun>,
    ) -> Option<FocusAction> {
        if !self.ready(snapshot) {
            return None;
        }
        let run = run?;
        if !self.session.background.desktop.connected()
            && let (Some(slot), Some(binding)) = (slot, &self.binding)
            && let Some(body) = snapshot.focus(slot, binding)
        {
            return Some(FocusAction::Companion(snapshot.clone(), Box::new(body)));
        }
        let key = snapshot.live_key(run)?;
        snapshot.desktop_run_target(key)?;
        Some(FocusAction::Desktop(key.clone()))
    }

    pub(super) fn card(
        &self,
        snapshot: &Arc<Snapshot>,
        slot: &PresentedSlot,
        index: usize,
        width: f32,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        self.run_card(snapshot, slot.run.as_ref(), Some(slot), index, width, cx)
    }

    pub(super) fn run_card(
        &self,
        snapshot: &Arc<Snapshot>,
        run: Option<&PresentedRun>,
        slot: Option<&PresentedSlot>,
        index: usize,
        width: f32,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let colors = self.colors;
        let cached = self.view.link != Link::Online || snapshot.generation().is_none();
        let action = self.focus_action(snapshot, slot, run);
        let height = if self.window.expanded {
            width
        } else {
            crate::shell::KEY_HEIGHT
        };
        let host = (!self.window.expanded && self.catalog_view.hosts.len() > 1)
            .then(|| self.catalog_view.hosts.get(&snapshot.host))
            .flatten()
            .map(|host| host.label.as_str());
        let face = colors.card_face(run, cached, self.selected == index, width, height, host);
        let mut card = self.attach_card_action(face, index, action, cx);
        if let Some(run) = run {
            card = card.when(self.menu.is_none(), |card| {
                RunDetails::attach(card, run, cx)
            });
            let mut captured = slot.cloned().unwrap_or_else(|| PresentedSlot {
                run: Some(run.clone()),
                ..Default::default()
            });
            captured.run = Some(run.clone());
            let snapshot = snapshot.clone();
            card = card.on_mouse_down(
                MouseButton::Right,
                cx.listener(move |view, _, _, cx| {
                    view.held.cancel();
                    view.menu = Some((snapshot.clone(), captured.clone()));
                    cx.notify();
                }),
            );
        }
        card
    }

    pub(super) fn attach_card_action(
        &self,
        face: Div,
        index: usize,
        action: Option<FocusAction>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let colors = self.colors;
        let mut card = face.id(("agent", index));
        if let Some(action) = action {
            card = card
                .cursor_pointer()
                .when(self.selected != index, |card| {
                    card.hover(|style| style.border_color(colors.muted))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _, _, cx| {
                        view.selected = index;
                        view.held.press(Input::Pointer, index, action.clone());
                        cx.notify();
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |view, _, _, cx| {
                        if let Some(action) = view.held.release(Input::Pointer, index) {
                            view.activate(action);
                        }
                        cx.notify();
                    }),
                );
        }
        card
    }

    pub(super) fn host_card(
        &self,
        host: &crate::catalog::Host,
        run: &PresentedRun,
        index: usize,
        width: f32,
        action: Option<FocusAction>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let colors = self.colors;
        let cached = !host.online;
        let face = colors.card_face(
            Some(run),
            cached,
            self.selected == index,
            width,
            if self.window.expanded {
                width
            } else {
                crate::shell::KEY_HEIGHT
            },
            Some(&host.label),
        );
        let card = self.attach_card_action(face, index, action, cx);
        RunDetails::attach(card, run, cx)
    }

    fn action_button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        snapshot: &Arc<Snapshot>,
        body: Option<Body>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let colors = self.colors;
        let enabled = body.is_some() && self.ready(snapshot);
        let snapshot = snapshot.clone();
        colors.button(id, label, enabled).when(enabled, |button| {
            button.on_click(cx.listener(move |view, _, _, cx| {
                if let Some(body) = &body {
                    view.submit(&snapshot, body.clone());
                }
                view.menu = None;
                cx.notify();
            }))
        })
    }

    pub(super) fn menu_view(&self, cx: &mut Context<Self>) -> Option<Div> {
        let colors = self.colors;
        let (snapshot, slot) = self.menu.as_ref()?;
        let run = slot.run.as_ref()?;
        let cached = self.view.link != Link::Online || snapshot.generation().is_none();
        let mut menu = column()
            .id("agent-actions")
            .p(px(16.))
            .w_full()
            .max_w(px(280.))
            .max_h_full()
            .overflow_y_scroll()
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(colors.panel)
            .child(
                colors
                    .describe(run, cached)
                    .child(colors.muted(placement_label(Some(slot)))),
            )
            .on_mouse_down_out(cx.listener(|view, _, _, cx| {
                view.menu = None;
                cx.notify();
            }));
        if slot.revision != 0 {
            menu = menu.child(self.action_button(
                "pin-agent",
                if slot.pinned { "Unpin" } else { "Pin" },
                snapshot,
                snapshot.pin(slot),
                cx,
            ));
        }
        if run.latest_unseen_attention_id.is_some() {
            menu = menu.child(self.action_button(
                "ack-agent",
                "Acknowledge alert",
                snapshot,
                snapshot.acknowledge(run),
                cx,
            ));
        }
        Some(
            div()
                .absolute()
                .occlude()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .p(px(8.))
                .bg(colors.scrim)
                .child(menu),
        )
    }

    pub(super) fn expanded(&mut self, window: &Window, cx: &mut Context<Self>) -> Div {
        let colors = self.colors;
        let mut content = column().flex_1().min_h_0().p(px(16.));
        if self.section == Section::Hosts {
            content = content.child(self.hosts(cx));
        } else if self.section == Section::Notifications {
            content = content.child(self.notifications(cx));
        } else if self.view.snapshot.is_some() || !self.deck_order.is_empty() {
            let snapshot = self.view.snapshot.clone();
            let sections = [
                (Section::Slots, "Deck".to_owned()),
                (Section::Overflow, format!("Queue ({})", self.queue_len())),
                (Section::History, "Run history".to_owned()),
            ];
            content = content.child(row().children(sections.into_iter().enumerate().map(
                |(index, (section, label))| {
                    colors
                        .button(("section", index), label, true)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.section = section;
                            view.held.cancel();
                            view.set_page(0);
                            cx.notify();
                        }))
                },
            )));
            content = content.child(match self.section {
                Section::Slots => snapshot.as_ref().map_or_else(
                    || column().child(colors.muted("Connecting to companion…")),
                    |snapshot| self.board(snapshot, window, cx),
                ),
                Section::Overflow => self.overflow(snapshot.as_ref(), cx),
                Section::History => column()
                    .children(
                        snapshot
                            .iter()
                            .flat_map(|snapshot| &snapshot.data.recent_ended)
                            .map(|run| {
                                colors.muted(format!("Run · {}", short_id(&run.agent_run_id)))
                            }),
                    )
                    .when(
                        snapshot
                            .as_ref()
                            .is_none_or(|snapshot| snapshot.data.recent_ended.is_empty()),
                        |view| view.child(colors.muted("No runs removed from the Deck yet.")),
                    ),
                _ => column(),
            });
        } else {
            content = content.child(colors.muted("Connecting to companion…"));
        }
        #[cfg(windows)]
        if self.pairing.is_open() {
            content = column().p(px(16.)).child(self.pair_form(cx));
        }
        column()
            .size_full()
            .gap_0()
            .child(self.window_header(cx))
            .child(self.header(cx))
            .child(
                div()
                    .id("management-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(content),
            )
            .child(self.footer(cx))
    }

    fn header(&self, cx: &mut Context<Self>) -> Div {
        let colors = self.colors;
        let mut header = column().flex_shrink_0().p(px(12.)).bg(colors.panel).child(
            row()
                .flex_wrap()
                .child(colors.button("hosts", "Hosts", true).on_click(
                    cx.listener(|view, _, window, cx| view.expand(Section::Hosts, window, cx)),
                ))
                .child(
                    colors
                        .button("notifications", "Notifications", true)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.expand(Section::Notifications, window, cx)
                        })),
                ),
        );
        if let Some(snapshot) = &self.view.snapshot {
            if let Some(code) = snapshot
                .data
                .issue_code
                .as_deref()
                .filter(|code| safe_code(code))
            {
                header = header.child(colors.muted(format!("Connection issue ({code})")));
            }
            if !self.session.background.desktop.connected() {
                header = header.child(colors.muted("Target terminal")).child(
                    row().flex_wrap().children(
                        snapshot
                            .data
                            .bindings
                            .iter()
                            .filter(|binding| snapshot.contains_binding(binding))
                            .enumerate()
                            .map(|(index, binding)| {
                                let binding = binding.clone();
                                let selected = self.binding.as_ref() == Some(&binding);
                                colors
                                    .button(
                                        ("binding", index),
                                        binding_label(&binding),
                                        self.ready(snapshot),
                                    )
                                    .when(selected, |button| button.text_color(colors.accent))
                                    .when(self.ready(snapshot), |button| {
                                        button.on_click(cx.listener(move |view, _, _, cx| {
                                            view.binding = Some(binding.clone());
                                            view.held.cancel();
                                            cx.notify();
                                        }))
                                    })
                            }),
                    ),
                );
                #[cfg(windows)]
                if snapshot.can_pair_wezterm {
                    header = header.child(
                        colors
                            .button("pair", "Pair WezTerm…", self.ready(snapshot))
                            .when(self.ready(snapshot), |button| {
                                button.on_click(cx.listener(|view, _, _, cx| {
                                    view.pairing.open(&view.session.signal);
                                    cx.notify();
                                }))
                            }),
                    );
                }
            }
        }
        header.child(self.layout_controls(cx))
    }

    fn board(&self, snapshot: &Arc<Snapshot>, window: &Window, cx: &mut Context<Self>) -> Div {
        let width = f32::from(window.viewport_size().width) - 32.;
        let columns = ((width + 12.) / 164.).floor().clamp(1., 6.) as usize;
        let edge = ((width - 12. * (columns - 1) as f32) / columns as f32).clamp(100., 212.);
        row().flex_wrap().gap(px(12.)).children(
            snapshot
                .data
                .slots
                .iter()
                .enumerate()
                .map(|(index, slot)| self.card(snapshot, slot, index, edge, cx)),
        )
    }

    fn overflow(&self, snapshot: Option<&Arc<Snapshot>>, cx: &mut Context<Self>) -> Div {
        let colors = self.colors;
        let mut view = column().child(
            row()
                .child(
                    colors
                        .button("previous", "Previous", self.page > 0)
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.set_page(view.page.saturating_sub(1));
                            view.held.cancel();
                            cx.notify();
                        })),
                )
                .child(colors.muted(format!("Page {} of {}", self.page + 1, self.queue_pages())))
                .child(
                    colors
                        .button("next", "Next", self.page + 1 < self.queue_pages())
                        .on_click(cx.listener(|view, _, _, cx| {
                            if view.page + 1 < view.queue_pages() {
                                view.set_page(view.page + 1);
                            }
                            view.held.cancel();
                            cx.notify();
                        })),
                ),
        );
        let tiles = self.queue_tiles();
        view = view.child(
            row().flex_wrap().children(
                tiles
                    .iter()
                    .enumerate()
                    .map(|(index, tile)| self.tile_card(*tile, index, 150., cx)),
            ),
        );
        let mut suppressed = false;
        if let Some(snapshot) = snapshot.filter(|snapshot| snapshot.page == self.page) {
            suppressed = !snapshot.data.suppressed.is_empty();
            for (index, run) in snapshot.data.suppressed.iter().enumerate() {
                view = view.child(self.action_button(
                    ("readd", index),
                    format!("Re-add {}", short_id(&run.agent_run_id)),
                    snapshot,
                    snapshot.readd(run),
                    cx,
                ));
            }
        }
        if tiles.is_empty() && !suppressed {
            view = view.child(colors.muted("No queued agents on this page."));
        }
        view
    }

    fn hosts(&mut self, cx: &mut Context<Self>) -> Div {
        let colors = self.colors;
        let mut content = column().child(
            colors
                .button("refresh-hosts", "Refresh hosts", true)
                .on_click(cx.listener(|view, _, _, cx| {
                    view.catalog = None;
                    view.refresh_view();
                    cx.notify();
                })),
        );
        let current = self.session.catalog.1;
        if let Some(Err(error)) = &self.catalog {
            return content.child(colors.muted(error.clone()));
        }
        let data = self.catalog_view.clone();
        if let Some(message) = data.message {
            content = content.child(colors.muted(message));
        }
        for (index, (id, host)) in data.hosts.into_iter().enumerate() {
            let mut entry = column().p(px(12.)).bg(colors.panel).rounded_lg().child(
                row()
                    .child(host.label.clone())
                    .child(colors.muted(if host.online {
                        "Live"
                    } else if host.runs.is_empty() {
                        "Offline"
                    } else {
                        "Cached"
                    }))
                    .child(
                        colors
                            .button(
                                ("open-host", index),
                                if id == current {
                                    "This Deck's slots"
                                } else {
                                    "Show host Deck"
                                },
                                true,
                            )
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if id == view.session.catalog.1 {
                                    view.section = Section::Slots;
                                } else if let Some(Ok(catalog)) = &view.catalog {
                                    catalog.open(id);
                                }
                                cx.notify();
                            })),
                    ),
            );
            for run in host.runs.iter().take(crate::catalog::HOST_PREVIEW_LIMIT) {
                entry = entry.child(colors.muted(format!(
                    "{} · {} · {} pending",
                    run.key
                        .as_ref()
                        .map_or_else(|| "unknown".into(), |key| short_id(&key.agent_run_id)),
                    badge(run.last_badge).0,
                    run.pending_requests
                )));
            }
            let omitted = host
                .runs
                .len()
                .saturating_sub(crate::catalog::HOST_PREVIEW_LIMIT);
            if omitted > 0 {
                entry = entry.child(colors.muted(format!("{omitted} additional runs in the Deck")));
            }
            content = content.child(entry);
        }
        content
    }

    fn notifications(&self, cx: &mut Context<Self>) -> Div {
        let colors = self.colors;
        let background = self.session.background.view.borrow().clone();
        let settings = &background.settings;
        let capabilities = background.capabilities;
        let choices = [
            ("Show OS notifications", settings.enabled),
            ("Questions and approvals", settings.input),
            ("New responses", settings.responses),
            ("Errors", settings.failures),
            ("Sound", settings.sound),
            ("Start Agent Deck at login", settings.start_at_login),
        ];
        let mut view = column()
            .children(
                choices
                    .into_iter()
                    .enumerate()
                    .filter(|(index, _)| match index {
                        4 => capabilities.sound.ready(),
                        5 => capabilities.login_startup.ready(),
                        _ => true,
                    })
                    .map(|(index, (label, enabled))| {
                        colors
                            .button(
                                ("setting", index),
                                format!("{} {label}", if enabled { "☑" } else { "☐" }),
                                true,
                            )
                            .on_click(cx.listener(move |view, _, _, cx| {
                                let mut settings =
                                    view.session.background.view.borrow().settings.clone();
                                let value = match index {
                                    0 => &mut settings.enabled,
                                    1 => &mut settings.input,
                                    2 => &mut settings.responses,
                                    3 => &mut settings.failures,
                                    4 => &mut settings.sound,
                                    _ => &mut settings.start_at_login,
                                };
                                *value = !*value;
                                view.session.background.configure(settings);
                                cx.notify();
                            }))
                    }),
            )
            .child(
                row()
                    .flex_wrap()
                    .child(
                        colors
                            .button(
                                "pause",
                                if settings.paused_until_ms > now_ms() {
                                    "Resume notifications"
                                } else {
                                    "Pause for 1 hour"
                                },
                                true,
                            )
                            .on_click(cx.listener(|view, _, _, cx| {
                                let mut settings =
                                    view.session.background.view.borrow().settings.clone();
                                settings.paused_until_ms = if settings.paused_until_ms > now_ms() {
                                    0
                                } else {
                                    now_ms().saturating_add(3_600_000)
                                };
                                view.session.background.configure(settings);
                                cx.notify();
                            })),
                    )
                    .child(
                        colors
                            .button(
                                "test-notification",
                                "Send test notification",
                                capabilities.notifications.ready(),
                            )
                            .when(capabilities.notifications.ready(), |button| {
                                button.on_click(cx.listener(|view, _, _, _| {
                                    view.session.background.test_notification()
                                }))
                            }),
                    )
                    .child(
                        colors
                            .button("quit", "Quit Corvid Agent Deck", true)
                            .on_click(cx.listener(|view, _, _, _| view.session.signal.quit())),
                    ),
            );
        if !background.status.is_empty() {
            view = view.child(colors.muted(background.status));
        }
        if let Some(notice) = &self.selected_notice {
            let focus_error = background
                .history
                .iter()
                .find(|entry| !notice.summary && entry.key == notice.key)
                .and_then(|entry| entry.focus_error.clone());
            let key = notice.key.run();
            let enabled = self.view.snapshot.as_ref().is_some_and(|snapshot| {
                !notice.summary
                    && self.ready(snapshot)
                    && snapshot.desktop_run_target(&key).is_some()
            });
            view = view.child(
                column()
                    .child(notice.title.clone())
                    .child(colors.muted(notice.body.clone()))
                    .when_some(focus_error, |detail, error| {
                        detail.child(colors.muted(error))
                    })
                    .child(
                        colors
                            .button(
                                "focus-notification",
                                "Open existing terminal in Desktop",
                                enabled,
                            )
                            .when(enabled, |button| {
                                button.on_click(cx.listener(move |view, _, _, cx| {
                                    view.focus_run(key.clone());
                                    cx.notify();
                                }))
                            }),
                    ),
            );
        }
        view = view.child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Recent notifications"),
        );
        if background.history.is_empty() {
            view = view.child(colors.muted("New agent notifications will appear here."));
        }
        view.children(
            background
                .history
                .into_iter()
                .enumerate()
                .map(|(index, notice)| {
                    column().child(
                        colors
                            .button(("notice", index), notice.title.clone(), true)
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.selected_notice = Some(notice.clone());
                                cx.notify();
                            })),
                    )
                }),
        )
    }
}

impl Colors {
    pub(super) fn card_face(
        self,
        run: Option<&PresentedRun>,
        cached: bool,
        selected: bool,
        width: f32,
        height: f32,
        host: Option<&str>,
    ) -> Div {
        let status = run.map(|run| status(run, cached).1);
        let color = status.map(|status| self.status(status));
        let face = div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .when(host.is_some(), |face| face.flex_col())
            .rounded_lg()
            .border_color(color.unwrap_or(self.border))
            .when(run.is_none(), |face| face.border_1())
            .when_some(color, |face, color| {
                let (top, bottom) = if status == Some(StatusColor::Muted) {
                    (0.118, 0.047)
                } else {
                    (0.227, 0.075)
                };
                face.border_2().bg(linear_gradient(
                    180.,
                    linear_color_stop(gpui::Rgba { a: top, ..color }, 0.),
                    linear_color_stop(gpui::Rgba { a: bottom, ..color }, 1.),
                ))
            })
            .when_some(run, |face, run| {
                face.child(
                    svg()
                        .path(provider_logo(run.provider))
                        .size(px((width.min(height) * 0.46).clamp(28., 72.)))
                        .text_color(if status == Some(StatusColor::Muted) {
                            self.muted
                        } else {
                            self.text
                        }),
                )
            })
            .when_some(host, |face, host| {
                face.child(
                    self.muted(host.to_owned())
                        .max_w_full()
                        .px(px(3.))
                        .text_size(px(10.))
                        .truncate(),
                )
            });

        // Selection surrounds the status border instead of replacing its color.
        div()
            .w(px(width))
            .h(px(height))
            .flex_shrink_0()
            .p(px(2.))
            .rounded(px(11.))
            .border_1()
            .border_color(if selected && run.is_some() {
                self.text
            } else {
                rgba(0x00000000)
            })
            .bg(if run.is_some() {
                self.panel
            } else {
                self.empty_card
            })
            .child(face)
    }
}

fn provider_logo(raw: i32) -> &'static str {
    match AgentProvider::try_from(raw) {
        Ok(AgentProvider::Claude) => chrome::CLAUDE,
        Ok(AgentProvider::Codex) => chrome::CODEX,
        _ => chrome::AGENT,
    }
}

fn placement_label(slot: Option<&PresentedSlot>) -> String {
    match slot.filter(|slot| slot.revision != 0) {
        Some(slot) => format!(
            "Slot {:02} · {}",
            slot.slot_id,
            if slot.pinned { "Pinned" } else { "Unpinned" },
        ),
        None => "Queued · No slot assigned".into(),
    }
}

struct RunDetails {
    deck: gpui::Entity<DeckView>,
    key: PresentedRunKey,
}

impl RunDetails {
    fn attach(card: Stateful<Div>, run: &PresentedRun, cx: &Context<DeckView>) -> Stateful<Div> {
        let Some(key) = run.key.clone() else {
            return card;
        };
        let deck = cx.entity();
        card.tooltip(move |_, cx| {
            cx.new(|cx| {
                cx.observe(&deck, |_, _, cx| cx.notify()).detach();
                Self {
                    deck: deck.clone(),
                    key: key.clone(),
                }
            })
            .into()
        })
    }

    fn content(&self, deck: &DeckView) -> Option<Div> {
        let host_id = Uuid::parse_str(&self.key.host_id).ok()?;
        if let Some(snapshot) = deck
            .view
            .snapshot
            .as_ref()
            .filter(|view| view.host == host_id)
        {
            let slot =
                snapshot.data.slots.iter().find(|slot| {
                    slot.run.as_ref().and_then(|run| run.key.as_ref()) == Some(&self.key)
                });
            let run = slot.and_then(|slot| slot.run.as_ref()).or_else(|| {
                snapshot
                    .data
                    .overflow
                    .iter()
                    .find(|run| run.key.as_ref() == Some(&self.key))
            });
            if let Some(run) = run {
                let cached = deck.view.link != Link::Online || snapshot.generation().is_none();
                return Some(
                    deck.colors
                        .describe(run, cached)
                        .child(deck.colors.muted(placement_label(slot))),
                );
            }
        }
        let host = deck.catalog_view.hosts.get(&host_id)?;
        let run = host
            .runs
            .iter()
            .find(|run| run.key.as_ref() == Some(&self.key))?;
        Some(
            deck.colors
                .describe(run, !host.online)
                .child(deck.colors.muted(host.label.clone())),
        )
    }
}

impl Render for RunDetails {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let deck = self.deck.read(cx);
        let colors = deck.colors;
        self.content(deck)
            .unwrap_or_else(|| colors.muted("Agent no longer displayed"))
            .p(px(12.))
            .bg(colors.panel)
            .text_color(colors.text)
            .text_size(px(12.))
            .border_1()
            .border_color(colors.border)
            .rounded_md()
    }
}

fn provider(run: &PresentedRun) -> &'static str {
    match AgentProvider::try_from(run.provider) {
        Ok(AgentProvider::Claude) => "Claude",
        Ok(AgentProvider::Codex) => "Codex",
        _ => "Agent",
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusColor {
    Muted,
    Working,
    Attention,
    Response,
    Error,
}

fn badge(raw: i32) -> (&'static str, StatusColor) {
    match AgentBadge::try_from(raw) {
        Ok(AgentBadge::Working) => ("Working", StatusColor::Working),
        Ok(AgentBadge::NeedsInput) => ("Needs input", StatusColor::Attention),
        Ok(AgentBadge::ResponseObserved) => ("New response", StatusColor::Response),
        Ok(AgentBadge::Ready) => ("Ready", StatusColor::Muted),
        Ok(AgentBadge::Interrupted) => ("Interrupted", StatusColor::Attention),
        Ok(AgentBadge::Error) => ("Error", StatusColor::Error),
        Ok(AgentBadge::Ended) => ("Ended", StatusColor::Muted),
        _ => ("Unknown", StatusColor::Muted),
    }
}
fn status(run: &PresentedRun, cached: bool) -> (&'static str, StatusColor) {
    if let Some(label) =
        availability_label(RunAvailability::try_from(run.availability).ok(), cached)
    {
        return (label, StatusColor::Muted);
    }
    if run.pending_requests == 0 && run.unverified_requests > 0 {
        return ("Check terminal", StatusColor::Attention);
    }
    match prompt_detail(run) {
        Some(PromptObservation::Submitted) => ("Working (inferred)", StatusColor::Working),
        Some(PromptObservation::Responded) => badge(AgentBadge::ResponseObserved as i32),
        _ => badge(run.last_badge),
    }
}
impl Colors {
    fn status(self, color: StatusColor) -> gpui::Rgba {
        match color {
            StatusColor::Muted => self.muted,
            StatusColor::Working => self.working,
            StatusColor::Attention => self.attention,
            StatusColor::Response => self.response,
            StatusColor::Error => self.error,
        }
    }

    fn describe(self, run: &PresentedRun, cached: bool) -> Div {
        let (label, color) = status(run, cached);
        let mut view = column()
            .child(provider(run))
            .child(div().text_color(self.status(color)).child(label))
            .child(self.muted(evidence_label(run)))
            .child(self.muted(format!(
                "{} pending · {} unverified · {} unseen",
                run.pending_requests, run.unverified_requests, run.unseen_attention
            )));
        if let Some(key) = &run.key {
            view = view.child(self.muted(short_id(&key.agent_run_id)));
        }
        if let Some(age) = run.last_observed_unix_ms.and_then(observation_age) {
            view = view.child(self.muted(format!("Observed {age} ago")));
        }
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available(badge: AgentBadge) -> PresentedRun {
        PresentedRun {
            availability: RunAvailability::Available as i32,
            last_badge: badge as i32,
            ..Default::default()
        }
    }

    #[test]
    fn live_prompt_observations_fill_unknown_status_until_stronger_evidence_arrives() {
        let mut run = available(AgentBadge::Unknown);
        run.collection = AgentCollection::Healthy as i32;
        assert_eq!(status(&run, false), ("Unknown", StatusColor::Muted));
        run.prompt_observation = PromptObservation::Submitted as i32;
        assert_eq!(
            status(&run, false),
            ("Working (inferred)", StatusColor::Working)
        );
        run.prompt_observation = PromptObservation::Responded as i32;
        assert_eq!(status(&run, false), ("New response", StatusColor::Response));
        assert_eq!(status(&run, true), ("Cached", StatusColor::Muted));
        run.collection = AgentCollection::GapDetected as i32;
        assert_eq!(status(&run, false), ("Unknown", StatusColor::Muted));
        run.collection = AgentCollection::Healthy as i32;
        run.unverified_requests = 1;
        assert_eq!(
            status(&run, false),
            ("Check terminal", StatusColor::Attention)
        );
        run.pending_requests = 1;
        run.last_badge = AgentBadge::NeedsInput as i32;
        assert_eq!(status(&run, false), ("Needs input", StatusColor::Attention));
    }

    #[test]
    fn only_observed_responses_use_the_completion_color() {
        for badge in [
            AgentBadge::Working,
            AgentBadge::NeedsInput,
            AgentBadge::ResponseObserved,
            AgentBadge::Ready,
            AgentBadge::Interrupted,
            AgentBadge::Error,
            AgentBadge::Ended,
            AgentBadge::Unknown,
            AgentBadge::Unspecified,
        ] {
            assert_eq!(
                status(&available(badge), false).1 == StatusColor::Response,
                badge == AgentBadge::ResponseObserved,
            );
        }
    }

    #[test]
    fn unavailable_observations_never_show_live_status_colors() {
        for badge in [
            AgentBadge::Working,
            AgentBadge::NeedsInput,
            AgentBadge::ResponseObserved,
        ] {
            let mut run = available(badge);
            assert_eq!(status(&run, true), ("Cached", StatusColor::Muted));
            for availability in [
                RunAvailability::Offline,
                RunAvailability::Historical,
                RunAvailability::CheckNeeded,
                RunAvailability::Unspecified,
            ] {
                run.availability = availability as i32;
                assert_eq!(status(&run, false).1, StatusColor::Muted);
            }
            run.availability = i32::MAX;
            assert_eq!(status(&run, false), ("Check needed", StatusColor::Muted));
        }
    }

    #[test]
    fn unverified_requests_override_an_observed_response() {
        let mut run = available(AgentBadge::ResponseObserved);
        run.unverified_requests = 1;
        assert_eq!(
            status(&run, false),
            ("Check terminal", StatusColor::Attention)
        );
        run.availability = RunAvailability::Offline as i32;
        assert_eq!(status(&run, false), ("Offline", StatusColor::Muted));
    }

    #[test]
    fn unknown_provider_and_badge_have_neutral_fallbacks() {
        assert_eq!(provider_logo(i32::MAX), chrome::AGENT);
        let mut run = available(AgentBadge::Unknown);
        run.last_badge = i32::MAX;
        assert_eq!(status(&run, false), ("Unknown", StatusColor::Muted));
    }
}
