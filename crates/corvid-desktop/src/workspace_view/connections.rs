use super::*;

impl WorkspaceView {
    pub(super) fn connections_panel(&self, ui: Option<&Ui>, cx: &mut Context<Self>) -> Div {
        let changing = self.connecting(ui);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .w_full()
            .min_h_0()
            .gap(px(12.))
            .p(px(14.))
            .bg(rgb(PANEL))
            .child(
                div()
                    .text_color(rgb(TEXT))
                    .text_size(px(19.))
                    .child("Connections"),
            )
            .child(
                div()
                    .id("connection-tree")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(12.))
                    .overflow_y_scroll()
                    .child(self.connection_branch(&Destination::Local, ui, changing, cx))
                    .when(cfg!(windows), |tree| {
                        #[cfg(windows)]
                        let tree = tree.child(self.wsl_list(ui, changing, cx));
                        tree
                    })
                    .child(self.host_list(ui, changing, cx)),
            )
            .when_some(self.error.clone(), |panel, error| {
                panel.child(div().text_size(px(13.)).text_color(rgb(MUTED)).child(error))
            })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        div()
                            .id("ssh-alias")
                            .track_focus(&self.alias_focus)
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .p(px(8.))
                            .rounded_md()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .text_color(rgb(TEXT))
                            .child(if self.alias.is_empty() {
                                "OpenSSH alias".into()
                            } else {
                                self.alias.clone()
                            })
                            .on_click(
                                cx.listener(|view, _, window, _| window.focus(&view.alias_focus)),
                            ),
                    )
                    .child(
                        button(
                            "connect",
                            if changing { "Connecting…" } else { "Connect" },
                            true,
                        )
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.connect_host(view.alias.clone(), window, cx)
                        })),
                    ),
            )
    }

    fn host_list(&self, ui: Option<&Ui>, changing: bool, cx: &mut Context<Self>) -> Div {
        let mut hosts: Vec<_> = self.hosts.as_ref().into_iter().flatten().cloned().collect();
        let mut extra: Vec<_> = self
            .known_sessions
            .keys()
            .chain(self.destination.iter())
            .filter_map(|destination| match destination {
                Destination::Ssh(alias) => Some(alias),
                _ => None,
            })
            .filter(|alias| !hosts.contains(alias))
            .cloned()
            .collect();
        extra.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        extra.dedup();
        hosts.extend(extra);
        let note = if self.host_scan.is_some() {
            Some("Reading SSH configuration…")
        } else {
            match &self.hosts {
                Err(_) => Some("Could not read the SSH configuration. Enter an alias below."),
                Ok(_) if hosts.is_empty() => Some("No SSH hosts found. Enter an alias below."),
                Ok(_) => None,
            }
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(5.))
            .w_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(rgb(TEXT))
                            .child("SSH Hosts"),
                    )
                    .child(
                        button("refresh-ssh-hosts", "Refresh", false)
                            .on_click(cx.listener(|view, _, _, cx| view.refresh_hosts(cx))),
                    ),
            )
            .when_some(note, |list, message| {
                list.child(
                    div()
                        .p(px(10.))
                        .rounded_md()
                        .bg(rgb(PANEL))
                        .text_size(px(13.))
                        .text_color(rgb(MUTED))
                        .child(message),
                )
            })
            .children(
                hosts.into_iter().map(|alias| {
                    self.connection_branch(&Destination::Ssh(alias), ui, changing, cx)
                }),
            )
    }

    #[cfg(windows)]
    fn wsl_list(&self, ui: Option<&Ui>, changing: bool, cx: &mut Context<Self>) -> Div {
        let mut distributions: Vec<_> = self
            .distributions
            .as_ref()
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        for destination in self.known_sessions.keys().chain(self.destination.iter()) {
            if let Destination::Wsl(distribution) = destination
                && !distributions.contains(distribution)
            {
                distributions.push(distribution.clone());
            }
        }
        distributions.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let note = if self.wsl_scan.is_some() {
            Some("Reading WSL distributions…".to_owned())
        } else {
            match &self.distributions {
                Err(error) => Some(error.to_string()),
                Ok(_) if distributions.is_empty() => {
                    Some("No WSL distributions installed.".to_owned())
                }
                Ok(_) => None,
            }
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(5.))
            .w_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(14.)).text_color(rgb(TEXT)).child("WSL"))
                    .child(
                        button("refresh-wsl", "Refresh", false)
                            .on_click(cx.listener(|view, _, _, cx| view.refresh_wsl(cx))),
                    ),
            )
            .when_some(note, |list, message| {
                list.child(
                    div()
                        .p(px(10.))
                        .text_size(px(13.))
                        .text_color(rgb(MUTED))
                        .child(message),
                )
            })
            .children(distributions.into_iter().map(|distribution| {
                self.connection_branch(&Destination::Wsl(distribution), ui, changing, cx)
            }))
    }

    fn connection_branch(
        &self,
        destination: &Destination,
        ui: Option<&Ui>,
        changing: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (id, label) = match destination {
            Destination::Local => ("local-terminal".to_owned(), "Local Terminal".to_owned()),
            Destination::Ssh(alias) => {
                (format!("ssh-{}", alias.as_str()), alias.as_str().to_owned())
            }
            #[cfg(windows)]
            Destination::Wsl(distribution) => (
                format!("wsl-{}", distribution.as_str()),
                distribution.as_str().to_owned(),
            ),
        };
        let current = ui.filter(|_| self.destination.as_ref() == Some(destination));
        let connected = current.is_some_and(|ui| ui.connected);
        let known = self.known_sessions.get(destination);
        let status = if connected {
            "Connected"
        } else if current.is_some_and(|ui| ui.changing) {
            "Connecting…"
        } else if known.is_some_and(|known| known.live) {
            "Live · detached"
        } else if known.is_some() {
            "Cached · reconnect ›"
        } else {
            "Connect ›"
        };
        let connection = destination.clone();
        div()
            .id(SharedString::from(id))
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(4.))
            .child(
                div()
                    .id("open-connection")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded_md()
                    .bg(rgb(if connected { ACTIVE_FILL } else { BACKGROUND }))
                    .text_color(rgb(TEXT))
                    .text_size(px(13.))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .when_some(known.filter(|known| known.live), |row, known| {
                        let agents = known
                            .inventory
                            .agents
                            .iter()
                            .filter(|agent| agent.terminal_available)
                            .count();
                        row.when(agents > 0, |row| {
                            row.child(div().flex_none().text_xs().text_color(rgb(MUTED)).child(
                                format!(
                                    "{agents} {}",
                                    if agents == 1 { "agent" } else { "agents" }
                                ),
                            ))
                        })
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(rgb(if connected { ACTIVE } else { MUTED }))
                            .child(status),
                    )
                    .when(!changing, |row| {
                        row.cursor_pointer()
                            .hover(|style| style.bg(rgb(ACTIVE_FILL)))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.switch_to(
                                    connection.clone(),
                                    AttachTarget::Default,
                                    window,
                                    cx,
                                )
                            }))
                    }),
            )
    }
}
