use super::*;
use gpui::svg;
use std::sync::Arc;

pub(super) const BACKGROUND: u32 = 0x0f1319;
pub(super) const PANEL: u32 = 0x151b23;
pub(super) const TEXT: u32 = 0xe5edf1;
pub(super) const MUTED: u32 = 0x8594a3;
pub(super) const MINT: u32 = 0x77d8b7;
pub(super) const AMBER: u32 = 0xf4c065;
pub(super) const RED: u32 = 0xf18590;
const BORDER: u32 = 0x303b48;

pub(super) fn column() -> Div {
    div().flex().flex_col().gap(px(8.))
}
pub(super) fn row() -> Div {
    div().flex().items_center().gap(px(8.))
}
pub(super) fn muted(text: impl Into<SharedString>) -> Div {
    div()
        .text_color(rgb(MUTED))
        .text_size(px(12.))
        .child(text.into())
}
pub(super) fn button(
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
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .text_color(rgb(if enabled { TEXT } else { MUTED }))
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x263541)))
        })
        .child(label.into())
}

impl DeckView {
    pub(super) fn focus_action(
        &self,
        snapshot: &Arc<Snapshot>,
        slot: Option<&PresentedSlot>,
        run: Option<&PresentedRun>,
    ) -> Option<FocusAction> {
        self.ready(snapshot)
            .then(|| {
                if self.session.background.desktop.connected() {
                    run.and_then(|run| snapshot.live_key(run))
                        .cloned()
                        .map(FocusAction::Desktop)
                } else {
                    slot.and_then(|slot| {
                        self.binding
                            .as_ref()
                            .and_then(|binding| snapshot.focus(slot, binding))
                    })
                    .map(|body| FocusAction::Companion(snapshot.clone(), Box::new(body)))
                }
            })
            .flatten()
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
        let cached = self.view.link != Link::Online || snapshot.generation().is_none();
        let action = self.focus_action(snapshot, slot, run);
        let height = if self.window.expanded {
            width
        } else {
            crate::shell::KEY_HEIGHT
        };
        let mut card =
            card_face(run, cached, self.selected == index, width, height).id(("agent", index));
        if let Some(action) = action {
            card = card
                .cursor_pointer()
                .when(self.selected != index, |card| {
                    card.hover(|style| style.border_color(rgb(MUTED)))
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
        if let Some(run) = run {
            let details = run.clone();
            let placement = placement_label(slot);
            card = card.when(self.menu.is_none(), |card| {
                card.tooltip(move |_, cx| {
                    cx.new(|_| RunDetails {
                        run: details.clone(),
                        cached,
                        placement: placement.clone(),
                    })
                    .into()
                })
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

    fn action_button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        snapshot: &Arc<Snapshot>,
        body: Option<Body>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let enabled = body.is_some() && self.ready(snapshot);
        let snapshot = snapshot.clone();
        button(id, label, enabled).when(enabled, |button| {
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
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(describe(run, cached).child(muted(placement_label(Some(slot)))))
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
                .bg(rgba(0x00000088))
                .child(menu),
        )
    }

    pub(super) fn expanded(&mut self, window: &Window, cx: &mut Context<Self>) -> Div {
        let mut content = column().flex_1().min_h_0().p(px(16.));
        if self.section == Section::Hosts {
            content = content.child(self.hosts(cx));
        } else if self.section == Section::Notifications {
            content = content.child(self.notifications(cx));
        } else if let Some(snapshot) = self.view.snapshot.clone() {
            let sections = [
                (Section::Slots, "Deck".to_owned()),
                (
                    Section::Overflow,
                    format!("Queue ({})", snapshot.data.overflow_count),
                ),
                (Section::History, "Closed panes".to_owned()),
            ];
            content = content.child(row().children(sections.into_iter().enumerate().map(
                |(index, (section, label))| {
                    button(("section", index), label, true).on_click(cx.listener(
                        move |view, _, _, cx| {
                            view.section = section;
                            view.held.cancel();
                            cx.notify();
                        },
                    ))
                },
            )));
            content =
                content.child(match self.section {
                    Section::Slots => self.board(&snapshot, window, cx),
                    Section::Overflow => self.overflow(&snapshot, cx),
                    Section::History => {
                        column()
                            .children(snapshot.data.recent_ended.iter().map(|run| {
                                muted(format!("Ended · {}", short_id(&run.agent_run_id)))
                            }))
                            .when(snapshot.data.recent_ended.is_empty(), |view| {
                                view.child(muted("No agents removed by pane closure yet."))
                            })
                    }
                    _ => column(),
                });
        } else {
            content = content.child(muted("Connecting to companion…"));
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
        let mut header = column().flex_shrink_0().p(px(12.)).bg(rgb(PANEL)).child(
            row()
                .flex_wrap()
                .child(button("hosts", "Hosts", true).on_click(
                    cx.listener(|view, _, window, cx| view.expand(Section::Hosts, window, cx)),
                ))
                .child(
                    button("notifications", "Notifications", true).on_click(cx.listener(
                        |view, _, window, cx| view.expand(Section::Notifications, window, cx),
                    )),
                ),
        );
        if let Some(snapshot) = &self.view.snapshot {
            if let Some(code) = snapshot
                .data
                .issue_code
                .as_deref()
                .filter(|code| safe_code(code))
            {
                header = header.child(muted(format!("Connection issue ({code})")));
            }
            if !self.session.background.desktop.connected() {
                header = header.child(muted("Target terminal")).child(
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
                                button(
                                    ("binding", index),
                                    binding_label(&binding),
                                    self.ready(snapshot),
                                )
                                .when(selected, |button| button.text_color(rgb(MINT)))
                                .when(
                                    self.ready(snapshot),
                                    |button| {
                                        button.on_click(cx.listener(move |view, _, _, cx| {
                                            view.binding = Some(binding.clone());
                                            view.held.cancel();
                                            cx.notify();
                                        }))
                                    },
                                )
                            }),
                    ),
                );
                #[cfg(windows)]
                if snapshot.can_pair_wezterm {
                    header =
                        header.child(button("pair", "Pair WezTerm…", self.ready(snapshot)).when(
                            self.ready(snapshot),
                            |button| {
                                button.on_click(cx.listener(|view, _, _, cx| {
                                    view.pairing.open(&view.session.signal);
                                    cx.notify();
                                }))
                            },
                        ));
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

    fn overflow(&self, snapshot: &Arc<Snapshot>, cx: &mut Context<Self>) -> Div {
        let mut view = column().child(
            row()
                .child(
                    button("previous", "Previous", self.page > 0).on_click(cx.listener(
                        |view, _, _, cx| {
                            view.set_page(view.page.saturating_sub(1));
                            view.held.cancel();
                            cx.notify();
                        },
                    )),
                )
                .child(muted(format!(
                    "Page {} of {}",
                    self.page + 1,
                    snapshot.page_count()
                )))
                .child(
                    button("next", "Next", self.page + 1 < snapshot.page_count()).on_click(
                        cx.listener(|view, _, _, cx| {
                            if let Some(snapshot) = &view.view.snapshot
                                && view.page + 1 < snapshot.page_count()
                            {
                                view.set_page(view.page + 1);
                            }
                            view.held.cancel();
                            cx.notify();
                        }),
                    ),
                ),
        );
        if snapshot.page != self.page {
            return view.child(muted("Loading page…"));
        }
        view = view.child(
            row().flex_wrap().children(
                snapshot
                    .data
                    .overflow
                    .iter()
                    .enumerate()
                    .map(|(index, run)| self.run_card(snapshot, Some(run), None, index, 150., cx)),
            ),
        );
        for (index, run) in snapshot.data.suppressed.iter().enumerate() {
            view = view.child(self.action_button(
                ("readd", index),
                format!("Re-add {}", short_id(&run.agent_run_id)),
                snapshot,
                snapshot.readd(run),
                cx,
            ));
        }
        if snapshot.data.overflow.is_empty() && snapshot.data.suppressed.is_empty() {
            view = view.child(muted("No queued agents on this page."));
        }
        view
    }

    fn hosts(&mut self, cx: &mut Context<Self>) -> Div {
        let mut content = column().child(button("refresh-hosts", "Refresh hosts", true).on_click(
            cx.listener(|view, _, _, cx| {
                view.catalog = None;
                cx.notify();
            }),
        ));
        let (state, current) = &self.session.catalog;
        let catalog = self.catalog.get_or_insert_with(|| {
            Catalog::start(state.clone(), *current, self.session.signal.clone())
                .map_err(|error| error.to_string())
        });
        let catalog = match catalog {
            Ok(catalog) => catalog,
            Err(error) => return content.child(muted(error.clone())),
        };
        let data = catalog.views.borrow().clone();
        if let Some(message) = data.message {
            content = content.child(muted(message));
        }
        for (index, (id, host)) in data.hosts.into_iter().enumerate() {
            let mut entry = column().p(px(12.)).bg(rgb(PANEL)).rounded_lg().child(
                row()
                    .child(host.label.clone())
                    .child(muted(if host.online {
                        "Live"
                    } else if host.runs.is_empty() {
                        "Offline"
                    } else {
                        "Cached"
                    }))
                    .child(
                        button(
                            ("open-host", index),
                            if id == *current {
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
            for run in &host.runs {
                entry = entry.child(muted(format!(
                    "{} · {} · {} pending",
                    run.key
                        .as_ref()
                        .map_or_else(|| "unknown".into(), |key| short_id(&key.agent_run_id)),
                    badge(run.last_badge).0,
                    run.pending_requests
                )));
            }
            if host.omitted > 0 {
                entry = entry.child(muted(format!("{} older runs omitted", host.omitted)));
            }
            content = content.child(entry);
        }
        content
    }

    fn notifications(&self, cx: &mut Context<Self>) -> Div {
        let background = self.session.background.view.borrow().clone();
        let settings = &background.settings;
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
                    .filter(|(index, _)| *index != 5 || background.background)
                    .map(|(index, (label, enabled))| {
                        button(
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
                        button(
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
                        button("test-notification", "Send test notification", true).on_click(
                            cx.listener(|view, _, _, _| {
                                view.session.background.test_notification()
                            }),
                        ),
                    )
                    .child(
                        button("quit", "Quit Corvid Agent Deck", true)
                            .on_click(cx.listener(|view, _, _, _| view.session.signal.quit())),
                    ),
            );
        if !background.status.is_empty() {
            view = view.child(muted(background.status));
        }
        if let Some(notice) = &self.selected_notice {
            let current = self.view.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.host == notice.key.host
                    && snapshot.data.remote_epoch.as_deref()
                        == Some(notice.key.epoch.to_string().as_str())
                    && snapshot.generation().is_some()
            });
            let enabled = current
                && self.session.background.desktop.connected()
                && !self.actions.is_pending();
            let key = notice.key.run();
            view = view.child(
                column()
                    .child(notice.title.clone())
                    .child(muted(notice.body.clone()))
                    .child(
                        button(
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
            view = view.child(muted("New agent notifications will appear here."));
        }
        view.children(
            background
                .history
                .into_iter()
                .enumerate()
                .map(|(index, notice)| {
                    column().child(
                        button(("notice", index), notice.title.clone(), true).on_click(
                            cx.listener(move |view, _, _, cx| {
                                view.selected_notice = Some(notice.clone());
                                cx.notify();
                            }),
                        ),
                    )
                }),
        )
    }
}

fn card_face(
    run: Option<&PresentedRun>,
    cached: bool,
    selected: bool,
    width: f32,
    height: f32,
) -> Div {
    let color = run.map(|run| status(run, cached).1);
    let face = div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .rounded_lg()
        .border_1()
        .border_color(rgb(color.unwrap_or(BORDER)))
        .when_some(color, |face, color| face.bg(rgba((color << 8) | 0x18)))
        .when_some(run, |face, run| {
            face.child(
                svg()
                    .path(provider_logo(run.provider))
                    .size(px((width.min(height) * 0.46).clamp(28., 72.)))
                    .text_color(rgb(if color == Some(MUTED) { MUTED } else { TEXT })),
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
            rgb(TEXT)
        } else {
            rgba(0x00000000)
        })
        .bg(rgb(if run.is_some() { PANEL } else { BACKGROUND }))
        .child(face)
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
    run: PresentedRun,
    cached: bool,
    placement: String,
}

impl Render for RunDetails {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        describe(&self.run, self.cached)
            .child(muted(self.placement.clone()))
            .p(px(12.))
            .bg(rgb(PANEL))
            .text_color(rgb(TEXT))
            .text_size(px(12.))
            .border_1()
            .border_color(rgb(BORDER))
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
fn badge(raw: i32) -> (&'static str, u32) {
    match AgentBadge::try_from(raw) {
        Ok(AgentBadge::Working) => ("Working", 0x78bdee),
        Ok(AgentBadge::NeedsInput) => ("Needs input", AMBER),
        Ok(AgentBadge::ResponseObserved) => ("New response", MINT),
        Ok(AgentBadge::Ready) => ("Ready", MUTED),
        Ok(AgentBadge::Interrupted) => ("Interrupted", AMBER),
        Ok(AgentBadge::Error) => ("Error", RED),
        Ok(AgentBadge::Ended) => ("Ended", MUTED),
        _ => ("Unknown", MUTED),
    }
}
fn status(run: &PresentedRun, cached: bool) -> (&'static str, u32) {
    if let Some(label) =
        availability_label(RunAvailability::try_from(run.availability).ok(), cached)
    {
        return (label, MUTED);
    }
    if run.pending_requests == 0 && run.unverified_requests > 0 {
        return ("Check terminal", AMBER);
    }
    badge(run.last_badge)
}
fn describe(run: &PresentedRun, cached: bool) -> Div {
    let (label, color) = status(run, cached);
    let mut view = column()
        .child(provider(run))
        .child(div().text_color(rgb(color)).child(label))
        .child(muted(evidence_label(run)))
        .child(muted(format!(
            "{} pending · {} unverified · {} unseen",
            run.pending_requests, run.unverified_requests, run.unseen_attention
        )));
    if let Some(key) = &run.key {
        view = view.child(muted(short_id(&key.agent_run_id)));
    }
    if let Some(age) = run.last_observed_unix_ms.and_then(observation_age) {
        view = view.child(muted(format!("Observed {age} ago")));
    }
    view
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
                status(&available(badge), false).1 == MINT,
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
            assert_eq!(status(&run, true), ("Cached", MUTED));
            for availability in [
                RunAvailability::Offline,
                RunAvailability::Historical,
                RunAvailability::CheckNeeded,
                RunAvailability::Unspecified,
            ] {
                run.availability = availability as i32;
                assert_eq!(status(&run, false).1, MUTED);
            }
            run.availability = i32::MAX;
            assert_eq!(status(&run, false), ("Check needed", MUTED));
        }
    }

    #[test]
    fn unverified_requests_override_an_observed_response() {
        let mut run = available(AgentBadge::ResponseObserved);
        run.unverified_requests = 1;
        assert_eq!(status(&run, false), ("Check terminal", AMBER));
        run.availability = RunAvailability::Offline as i32;
        assert_eq!(status(&run, false), ("Offline", MUTED));
    }

    #[test]
    fn unknown_provider_and_badge_have_neutral_fallbacks() {
        assert_eq!(provider_logo(i32::MAX), chrome::AGENT);
        let mut run = available(AgentBadge::Unknown);
        run.last_badge = i32::MAX;
        assert_eq!(status(&run, false), ("Unknown", MUTED));
    }
}
