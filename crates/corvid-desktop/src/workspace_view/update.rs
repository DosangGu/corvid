use super::*;
use anyhow::Result;

impl WorkspaceView {
    pub(super) fn update_menu(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(10.))
            .px(px(10.))
            .pb(px(6.))
            .bg(rgb(PANEL))
            .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child(
                if self.update_status.is_empty() {
                    format!("Corvid {}", env!("CARGO_PKG_VERSION"))
                } else {
                    self.update_status.clone()
                },
            ))
            .child(
                button("check-updates", "Check for updates…", false).on_click(
                    cx.listener(|view, _, window, cx| view.check_for_updates(window, cx)),
                ),
            )
    }

    fn check_for_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.release_update.is_some() {
            return;
        }
        self.update_status = "Checking for updates…".into();
        let check = cx
            .background_executor()
            .spawn(async { corvid_update::check(None) });
        self.release_update = Some(cx.spawn_in(window, async move |view, cx| {
            let outcome: Result<()> = async {
                let release = check.await?;
                if !release.is_newer_than(env!("CARGO_PKG_VERSION"))? {
                    view.update_in(cx, |view, _, cx| {
                        view.update_status = format!(
                            "Corvid {} is up to date.",
                            env!("CARGO_PKG_VERSION")
                        );
                        cx.notify();
                    })?;
                    return Ok(());
                }
                let installation = corvid_update::Installation::current();
                let detail = match &installation {
                    Ok(_) => "Install this release and restart Desktop. Running daemons are left unchanged.",
                    Err(_) => "This build is managed outside the Corvid installer. Open the release to download the new version.",
                };
                let notes = release.notes.chars().take(240).collect::<String>();
                let message = prompt_text(&format!(
                    "Installed: {}\nAvailable: {}\n\n{detail}\n\n{notes}",
                    env!("CARGO_PKG_VERSION"), release.version
                ));
                let response = view.update_in(cx, |view, window, cx| {
                    view.update_status = format!("Corvid {} is available.", release.version);
                    cx.notify();
                    let action = if installation.is_ok() {
                        "Install and restart"
                    } else {
                        "Open release"
                    };
                    let buttons = if installation.is_ok() {
                        vec!["Later", action, "Release notes"]
                    } else {
                        vec!["Later", action]
                    };
                    window.prompt(
                        PromptLevel::Info,
                        "Corvid update available",
                        Some(&message),
                        &buttons,
                        cx,
                    )
                })?;
                let decision = response.await;
                if decision == Ok(2) {
                    view.update_in(cx, |_, _, cx| cx.open_url(&release.url()))?;
                    return Ok(());
                }
                if decision != Ok(1) {
                    return Ok(());
                }

                let installation = match installation {
                    Ok(installation) => installation,
                    Err(_) => {
                        view.update_in(cx, |_, _, cx| cx.open_url(&release.url()))?;
                        return Ok(());
                    }
                };
                view.update_in(cx, |view, _, cx| {
                    view.update_status = "Downloading and installing…".into();
                    cx.notify();
                })?;
                let install = cx.background_executor()
                    .spawn(async move { installation.install(&release) });
                let installed = install.await?;
                // Release the old writer connection before the replacement app attaches.
                view.update_in(cx, |view, _, _| view.lifetime.shutdown())?;
                installed.start_desktop()?;
                view.update_in(cx, |_, _, cx| cx.quit())?;
                Ok(())
            }.await;
            let response = view.update_in(cx, |view, window, cx| {
                view.release_update = None;
                let response = outcome.err().map(|error| {
                    view.update_status = "Update failed. Your existing installation is available.".into();
                    window.prompt(
                        PromptLevel::Warning,
                        "Corvid update failed",
                        Some(&prompt_text(&format!("{error:#}"))),
                        &["OK"],
                        cx,
                    )
                });
                cx.notify();
                response
            });
            if let Ok(Some(response)) = response {
                let _ = response.await;
            }
        }));
        cx.notify();
    }
}

// GPUI's fallback prompt has a narrow body. Bound and wrap text on both platforms.
fn prompt_text(text: &str) -> String {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && line.chars().count() + word.chars().count() + 1 > 34 {
                lines.push(std::mem::take(&mut line));
            }
            for character in word.chars() {
                if line.chars().count() == 34 {
                    lines.push(std::mem::take(&mut line));
                }
                line.push(character);
            }
            line.push(' ');
        }
        lines.push(line.trim_end().to_owned());
    }
    if lines.len() > 18 {
        lines.truncate(18);
        lines.push("…".into());
    }
    lines.join("\n")
}
