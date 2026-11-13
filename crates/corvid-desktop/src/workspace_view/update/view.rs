use std::time::UNIX_EPOCH;

use super::*;

impl WorkspaceView {
    pub(in crate::workspace_view) fn update_menu(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.update;
        let header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(gpui::img(crate::app_icon::image()).size(px(40.)))
                    .child(div().text_size(px(17.)).child("Corvid Desktop")),
            )
            .child(
                div()
                    .text_color(rgb(ACTIVE))
                    .child(format!("v{}", env!("CARGO_PKG_VERSION"))),
            );
        let mut card = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(16.))
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(header)
            .child(hint(format!(
                "{} · Stable channel",
                state.installation_label()
            )))
            .child(
                div()
                    .text_color(rgb(if state.error.is_some() {
                        0xeeb47b
                    } else {
                        TEXT
                    }))
                    .child(status(state)),
            );

        if let Some(checked) = state.last_checked
            && let Ok(since_epoch) = checked.duration_since(UNIX_EPOCH)
        {
            let seconds = since_epoch.as_secs();
            card = card.child(hint(format!(
                "Last checked at {:02}:{:02} UTC",
                (seconds / 3600) % 24,
                (seconds / 60) % 60,
            )));
        }
        if let Phase::Installing(stage) = state.phase {
            card = card.child(installation_steps(stage));
        }
        card = card.child(self.update_actions(cx));
        if let Some(release) = &state.release
            && (state.phase == Phase::Current
                || state
                    .installation
                    .as_ref()
                    .ok()
                    .is_some_and(Option::is_some))
        {
            let url = release.url();
            card = card.child(
                button("release-notes", "View release notes ↗", false)
                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))),
            );
        }
        if let Some(detail) = state
            .error
            .as_ref()
            .map(|error| &error.detail)
            .or_else(|| state.installation.as_ref().err())
        {
            card = card.child(
                button(
                    "update-details",
                    if state.details_open {
                        "Hide details"
                    } else {
                        "Show details"
                    },
                    false,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.update.details_open = !view.update.details_open;
                    cx.notify();
                })),
            );
            if state.details_open {
                card = card.child(hint(wrap_detail(detail)));
            }
        }
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(card)
            .child(hint(installation_hint(state)))
            .child(hint(
                "Server updates are managed separately for each connection.",
            ))
    }

    fn update_actions(&self, cx: &mut Context<Self>) -> Div {
        let state = &self.update;
        let mut actions = div().flex().flex_wrap().items_center().gap(px(8.));
        match state.phase {
            Phase::Checking | Phase::Installing(_) => {
                actions.child(hint("You can close Settings and keep working."))
            }
            Phase::Ready => {
                actions
                    .child(button("restart-update", "Restart to apply", true).on_click(
                        cx.listener(|view, _, window, cx| {
                            view.restart_updated_desktop(window, cx);
                        }),
                    ))
                    .child(
                        button("defer-update", "Later", false)
                            .on_click(cx.listener(|_, _, window, _| window.remove_window())),
                    )
            }
            _ => {
                if state.phase == Phase::Available
                    && let Some(release) = &state.release
                {
                    match &state.installation {
                        Ok(Some(_)) => {
                            actions = actions.child(
                                button(
                                    "download-update",
                                    if state.error.is_some() {
                                        "Retry update"
                                    } else {
                                        "Download update"
                                    },
                                    true,
                                )
                                .on_click(cx.listener(|view, _, _, cx| view.download_update(cx))),
                            );
                        }
                        _ => {
                            let url = release.url();
                            actions = actions.child(
                                button("open-update-release", "Open release ↗", true)
                                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))),
                            );
                        }
                    }
                }
                actions.child(
                    button(
                        "check-updates",
                        if state.error.is_some() && state.phase == Phase::Idle {
                            "Try again"
                        } else {
                            "Check for updates"
                        },
                        state.phase == Phase::Idle,
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.check_for_updates(cx))),
                )
            }
        }
    }
}

fn hint(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(rgb(MUTED))
        .child(text.into())
}

fn status(state: &State) -> String {
    if let Some(error) = &state.error {
        return error.summary.into();
    }
    let version = state
        .release
        .as_ref()
        .map(|release| release.version.to_string())
        .unwrap_or_default();
    match state.phase {
        Phase::Idle => "Check for a new version when you are ready.".into(),
        Phase::Checking => "Checking for updates…".into(),
        Phase::Current => "You are up to date.".into(),
        Phase::Available => format!("Version {version} is available."),
        Phase::Installing(InstallProgress::Downloading) => {
            format!("Downloading version {version}…")
        }
        Phase::Installing(InstallProgress::Verifying) => "Verifying the downloaded files…".into(),
        Phase::Installing(InstallProgress::Installing) => "Installing the update…".into(),
        Phase::Ready => format!("Version {version} is ready. Restart when it suits you."),
    }
}

fn installation_hint(state: &State) -> &'static str {
    if state.phase == Phase::Ready {
        if state
            .installation
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .is_some_and(|install| install.root.is_none())
        {
            return "Restart to open the updated managed copy. Your original portable directory is kept. Terminal jobs keep running.";
        }
        return "Restarting Desktop keeps your terminal jobs running. You can also apply the update the next time you open Corvid.";
    }
    match &state.installation {
        Ok(Some(install)) if install.root.is_none() => {
            "Updating installs a managed copy in Corvid's default location. Your portable copy is kept. You choose when to restart."
        }
        Ok(Some(_)) => {
            "Download and prepare the update while you work. You choose when to restart Desktop; terminal jobs keep running."
        }
        Ok(None) => {
            "Update this build using the same tool or source checkout you used to install it."
        }
        Err(_) => "Review the installation details before updating with your original installer.",
    }
}

fn installation_steps(current: InstallProgress) -> Div {
    div().flex().flex_wrap().gap(px(8.)).children(
        [
            (InstallProgress::Downloading, "1  Download"),
            (InstallProgress::Verifying, "2  Verify"),
            (InstallProgress::Installing, "3  Install"),
        ]
        .into_iter()
        .map(|(stage, label)| {
            div()
                .px(px(8.))
                .py(px(4.))
                .rounded_md()
                .text_size(px(12.))
                .bg(rgb(if current == stage {
                    ACTIVE_FILL
                } else {
                    BACKGROUND
                }))
                .text_color(rgb(if current == stage { ACTIVE } else { MUTED }))
                .child(label)
        }),
    )
}

fn wrap_detail(detail: &str) -> String {
    let mut wrapped = String::new();
    let mut column = 0;
    for character in detail.chars().take(4096) {
        if character == '\n' || column == 48 {
            wrapped.push('\n');
            column = 0;
        }
        if character != '\n' {
            wrapped.push(character);
            column += 1;
        }
    }
    wrapped
}
