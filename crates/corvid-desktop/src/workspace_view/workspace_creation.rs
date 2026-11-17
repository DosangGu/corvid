use std::ops::Range;

use corvid_protocol::wire::{BrowseDirectories, DirectoryListing, WorkspaceCreation};
use gpui::{ElementInputHandler, MouseButton, UTF16Selection, canvas};
use tokio::sync::{mpsc, watch};

use super::switcher::machine_label;
use super::*;
use crate::daemon_prompt::{self, Prompt, Prompts};

const MAX_PATH: usize = 4096;
const RECENT_LIMIT: usize = 5;

mod browser;

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Host,
    Folder,
    Name,
}

#[derive(Default)]
pub(super) struct Text {
    value: String,
    marked: Option<Range<usize>>,
    anchor: usize,
    cursor: usize,
}

impl Text {
    fn set(&mut self, value: String) {
        let end = value.len();
        *self = Self {
            value,
            anchor: end,
            cursor: end,
            marked: None,
        };
    }

    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

    fn utf16(&self, offset: usize) -> usize {
        self.value[..offset].encode_utf16().count()
    }

    fn byte(&self, index: usize) -> usize {
        let mut units = 0;
        for (offset, ch) in self.value.char_indices() {
            if units >= index {
                return offset;
            }
            units += ch.len_utf16();
        }
        self.value.len()
    }

    fn prior(&self) -> usize {
        self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.cursor
            + self.value[self.cursor..]
                .chars()
                .next()
                .map_or(0, char::len_utf8)
    }

    fn move_to(&mut self, position: usize, extend: bool) {
        self.cursor = position;
        if !extend {
            self.anchor = position;
        }
    }

    pub(super) fn content(&self, range: Range<usize>, actual: &mut Option<Range<usize>>) -> String {
        let start = self.byte(range.start);
        let end = self.byte(range.end);
        *actual = Some(self.utf16(start)..self.utf16(end));
        self.value[start..end].to_owned()
    }

    pub(super) fn selected(&self) -> UTF16Selection {
        let selected = self.selection();
        UTF16Selection {
            range: self.utf16(selected.start)..self.utf16(selected.end),
            reversed: self.cursor < self.anchor,
        }
    }

    pub(super) fn marked(&self) -> Option<Range<usize>> {
        let marked = self.marked.as_ref()?;
        Some(self.utf16(marked.start)..self.utf16(marked.end))
    }

    fn replace(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        composing: bool,
    ) {
        let range = range
            .map(|range| self.byte(range.start)..self.byte(range.end))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection());
        let budget = MAX_PATH - (self.value.len() - range.len());
        let text: String = text
            .chars()
            .filter(|ch| !ch.is_control())
            .scan(0, |len, ch| {
                *len += ch.len_utf8();
                (*len <= budget).then_some(ch)
            })
            .collect();
        let end = range.start + text.len();
        self.value.replace_range(range.clone(), &text);
        self.marked = (composing && !text.is_empty()).then_some(range.start..end);
        self.anchor = end;
        self.cursor = end;
        if let Some(selected) = selected {
            let start = self.utf16(range.start);
            let len = text.encode_utf16().count();
            self.anchor = self.byte(start + selected.start.min(len));
            self.cursor = self.byte(start + selected.end.min(len));
        }
    }
}

struct Prepared {
    endpoint: ControlEndpoint,
    host: String,
    supports_directory: bool,
}

pub(super) struct Draft {
    destination: Destination,
    host: Text,
    folder: Text,
    name: Text,
    field: Field,
    default_name: String,
    name_edited: bool,
    hidden: bool,
    expanded: bool,
    prepared: Option<Prepared>,
    browser: Option<Task<()>>,
    queries: Option<watch::Sender<(u64, BrowseDirectories)>>,
    revision: u64,
    listing: Option<DirectoryListing>,
    choice: usize,
    status: Option<String>,
    connecting: bool,
    validating: bool,
    creating: Option<Option<String>>,
}

impl Draft {
    pub(super) fn text(&self) -> &Text {
        match self.field {
            Field::Host => &self.host,
            Field::Folder => &self.folder,
            Field::Name => &self.name,
        }
    }

    fn text_mut(&mut self) -> &mut Text {
        match self.field {
            Field::Host => &mut self.host,
            Field::Folder => &mut self.folder,
            Field::Name => &mut self.name,
        }
    }

    fn propose_name(&mut self) {
        if self.name_edited {
            return;
        }
        let name = self
            .folder
            .value
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty() && !matches!(*name, "~" | "." | ".."))
            .map_or_else(|| self.default_name.clone(), str::to_owned);
        self.name.set(name);
    }

    fn query(&mut self, complete: bool) {
        self.choice = 0;
        self.listing = None;
        self.status = None;
        self.validating = false;
        self.revision += 1;
        if self.text().marked.is_some() {
            return;
        }
        if let Some(queries) = &self.queries {
            queries.send_replace((
                self.revision,
                BrowseDirectories {
                    path: self.folder.value.clone(),
                    complete,
                    show_hidden: self.hidden,
                    validate_only: false,
                },
            ));
        }
    }

    fn set_folder(&mut self, path: String) {
        self.field = Field::Folder;
        self.expanded = true;
        self.folder.set(path);
        self.propose_name();
        self.query(false);
    }

    fn edited(&mut self) {
        match self.field {
            Field::Host => self.choice = 0,
            Field::Folder => {
                self.expanded = true;
                self.propose_name();
                self.query(true);
            }
            Field::Name => self.name_edited = true,
        }
    }
}

impl WorkspaceView {
    pub(super) fn new_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_overlay(window, cx);
        let ui = self.session.as_ref().map(Session::ui);
        let count = ui
            .as_ref()
            .and_then(|ui| ui.inventory.as_ref())
            .map_or(0, |inventory| inventory.workspaces.len());
        let name = format!("Workspace {}", count + 1);
        let mut draft = Draft {
            destination: self.destination.clone().unwrap_or(Destination::Local),
            host: Text::default(),
            folder: Text::default(),
            name: Text::default(),
            field: Field::Host,
            default_name: name.clone(),
            name_edited: false,
            hidden: false,
            expanded: false,
            prepared: None,
            browser: None,
            queries: None,
            revision: 0,
            listing: None,
            choice: 0,
            status: None,
            connecting: false,
            validating: false,
            creating: None,
        };
        draft.name.set(name);
        self.creation = Some(draft);
        self.refresh_hosts(cx);
        #[cfg(windows)]
        self.refresh_wsl(cx);
        window.focus(&self.setup_focus);
        if ui.is_some_and(|ui| ui.connected && ui.endpoint.is_some()) {
            self.prepare_folder_host(false, window, cx);
        }
        cx.notify();
    }

    fn host_choices(&self, filter: &str) -> Vec<Destination> {
        let query = filter.to_lowercase();
        std::iter::once(Destination::Local)
            .chain(self.ssh_destinations())
            .chain({
                #[cfg(windows)]
                {
                    self.wsl_destinations()
                }
                #[cfg(not(windows))]
                {
                    std::iter::empty()
                }
            })
            .filter(|target| machine_label(target).to_lowercase().contains(&query))
            .collect()
    }

    fn choose_host(
        &mut self,
        destination: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = self.creation.as_mut().expect("workspace editor");
        if draft.creating.is_some() {
            return;
        }
        draft.destination = destination;
        draft.host.set(String::new());
        draft.folder.set(String::new());
        draft.propose_name();
        draft.expanded = false;
        self.prepare_folder_host(true, window, cx);
    }

    fn prepare_folder_host(&mut self, explicit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.creation.as_mut().expect("workspace editor");
        let destination = draft.destination.clone();
        let current = self
            .session
            .as_ref()
            .map(Session::ui)
            .filter(|ui| ui.connected && self.destination.as_ref() == Some(&destination));
        let known_endpoint = current.as_ref().and_then(|ui| ui.endpoint.clone());
        let expected_host = current
            .and_then(|ui| ui.host_id)
            .or_else(|| {
                self.known_sessions
                    .get(&destination)
                    .map(|known| known.host_id.clone())
            })
            .or_else(|| {
                self.store
                    .for_target(&destination.saved_target())
                    .map(|saved| saved.host_id.to_string())
            });
        draft.prepared = None;
        draft.browser = None;
        draft.queries = None;
        draft.validating = false;
        draft.listing = None;
        draft.connecting = true;
        draft.status = Some(format!("Connecting to {}…", machine_label(&destination)));
        let result = browser::start(
            destination,
            self.agent_deck.state.clone(),
            known_endpoint,
            expected_host,
            explicit,
        );
        let (queries, mut updates, mut prompts) = match result {
            Ok(channels) => channels,
            Err(error) => {
                draft.connecting = false;
                draft.status = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        draft.queries = Some(queries);
        draft.browser = Some(cx.spawn_in(window, async move |view, cx| {
            loop {
                tokio::select! {
                    update = updates.recv() => {
                        let Some(update) = update else { break };
                        if view.update_in(cx, |view, window, cx| {
                            if let Some(draft) = &mut view.creation {
                                if draft.update(update) {
                                    view.finish_workspace(window, cx);
                                }
                                cx.notify();
                            }
                        }).is_err() {
                            break;
                        }
                    }
                    Some(prompt) = prompts.recv() => {
                        let answer = view.update_in(cx, |_, window, cx| prompt.show(window, cx));
                        if let Ok(answer) = answer {
                            answer.await;
                        }
                    }
                }
            }
        }));
        cx.notify();
    }

    pub(super) fn edit_creation(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        composing: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = &mut self.creation
            && draft.creating.is_none()
        {
            draft.text_mut().replace(range, text, selected, composing);
            draft.edited();
            cx.notify();
        }
    }

    pub(super) fn unmark_creation(&mut self, cx: &mut Context<Self>) {
        if let Some(draft) = &mut self.creation
            && draft.text_mut().marked.take().is_some()
        {
            draft.edited();
            cx.notify();
        }
    }

    pub(super) fn creation_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke;
        let draft = self.creation.as_ref().expect("workspace editor");
        if draft.creating.is_some() {
            cx.stop_propagation();
            return;
        }
        let field = draft.field;
        let marked = draft.text().marked.is_some();
        let command = key.modifiers.control || key.modifiers.platform;
        if !marked && matches!(key.key.as_str(), "enter" | "tab") {
            cx.stop_propagation();
            self.accept_creation_input(field, key.key == "tab", window, cx);
            return;
        }
        if !marked && key.key == "escape" {
            cx.stop_propagation();
            self.dismiss_overlay(window, cx);
            return;
        }
        if !marked && matches!(key.key.as_str(), "up" | "down") && field != Field::Name {
            let length = match field {
                Field::Host => self.host_choices(&draft.host.value).len(),
                Field::Folder => draft.listing.as_ref().map_or(0, |list| list.entries.len()),
                Field::Name => 0,
            };
            let draft = self.creation.as_mut().expect("workspace editor");
            draft.choice = if key.key == "up" {
                draft.choice.saturating_sub(1)
            } else {
                (draft.choice + 1).min(length.saturating_sub(1))
            };
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if marked {
            return;
        }
        let draft = self.creation.as_mut().expect("workspace editor");
        let edit = draft.text_mut();
        let mut changed = false;
        match key.key.as_str() {
            "a" if command => {
                edit.anchor = 0;
                edit.cursor = edit.value.len();
            }
            "v" if command => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    edit.replace(None, &text, None, false);
                    changed = true;
                }
            }
            "left" => edit.move_to(edit.prior(), key.modifiers.shift),
            "right" => edit.move_to(edit.next(), key.modifiers.shift),
            "home" => edit.move_to(0, key.modifiers.shift),
            "end" => edit.move_to(edit.value.len(), key.modifiers.shift),
            "backspace" | "delete" => {
                if edit.selection().is_empty() {
                    edit.anchor = if key.key == "backspace" {
                        edit.prior()
                    } else {
                        edit.next()
                    };
                }
                edit.replace(None, "", None, false);
                changed = true;
            }
            _ => return,
        }
        if changed {
            draft.edited();
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn accept_creation_input(
        &mut self,
        field: Field,
        tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = self.creation.as_ref().expect("workspace editor");
        match field {
            Field::Host => {
                if let Some(host) = self
                    .host_choices(&draft.host.value)
                    .into_iter()
                    .nth(draft.choice)
                {
                    self.choose_host(host, window, cx);
                } else {
                    self.connect_typed_host(window, cx);
                }
            }
            Field::Folder if tab => {
                let draft = self.creation.as_mut().expect("workspace editor");
                if let Some(entry) = draft
                    .listing
                    .as_ref()
                    .and_then(|list| list.entries.get(draft.choice))
                {
                    draft.set_folder(entry.path.clone());
                } else {
                    draft.field = Field::Name;
                    draft.name.anchor = 0;
                }
                cx.notify();
            }
            _ => self.submit_workspace(window, cx),
        }
    }

    fn connect_typed_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.creation.as_mut().expect("workspace editor");
        match SshAlias::new(draft.host.value.clone()) {
            Ok(alias) => self.choose_host(Destination::Ssh(alias), window, cx),
            Err(_) => {
                draft.status = Some(
                    "Choose a host or enter an OpenSSH alias from your SSH configuration.".into(),
                );
                cx.notify();
            }
        }
    }

    fn submit_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.creation.as_mut().expect("workspace editor");
        if draft.prepared.is_none()
            || draft.creating.is_some()
            || draft.validating
            || draft.text().marked.is_some()
        {
            return;
        }
        if !draft.folder.value.is_empty()
            && let Some(queries) = &draft.queries
        {
            draft.validating = true;
            draft.revision += 1;
            draft.status = Some("Checking the workspace folder…".into());
            queries.send_replace((
                draft.revision,
                BrowseDirectories {
                    path: draft.folder.value.clone(),
                    validate_only: true,
                    ..Default::default()
                },
            ));
            cx.notify();
            return;
        }
        self.finish_workspace(window, cx);
    }

    fn finish_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.creation.as_mut().expect("workspace editor");
        let Some(prepared) = &draft.prepared else {
            return;
        };
        let label = corvid_core::layout::clean_title(&draft.name.value);
        if label.is_empty() {
            draft.status = Some("Enter a workspace name.".into());
            cx.notify();
            return;
        }
        let directory = (!draft.folder.value.is_empty()).then(|| draft.folder.value.clone());
        if directory.is_some() && !prepared.supports_directory {
            return;
        }
        let old = self.selected.as_ref().map(|view| view.workspace_id.clone());
        draft.creating = Some(old);
        draft.status = Some("Creating workspace…".into());
        if self.destination.as_ref() == Some(&draft.destination)
            && let Some(session) = self.session.as_ref().filter(|session| {
                let ui = session.ui();
                ui.connected && !ui.changing && ui.host_id.as_ref() == Some(&prepared.host)
            })
        {
            session.create(match directory {
                Some(directory) => Target::Workspace(WorkspaceCreation { label, directory }),
                None => Target::WorkspaceLabel(label),
            });
        } else {
            let destination = draft.destination.clone();
            let target = AttachTarget::NewWorkspace {
                label,
                directory,
                endpoint: prepared.endpoint.clone(),
                host_id: prepared.host.clone(),
            };
            self.switch_to(destination, target, window, cx);
        }
        cx.notify();
    }

    pub(super) fn sync_creation(&mut self, ui: &Ui, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &mut self.creation else {
            return;
        };
        let Some(old) = &draft.creating else { return };
        if ui.changing || self.host_switch.is_some() {
            return;
        }
        if let Some(selected) = ui.selected.as_ref().filter(|_| ui.connected)
            && Some(&selected.workspace_id) != old.as_ref()
        {
            if let Some(host) = &ui.host_id
                && !draft.folder.value.is_empty()
            {
                let paths = self.recent_folders.entry(host.clone()).or_default();
                paths.retain(|path| path != &draft.folder.value);
                paths.insert(0, draft.folder.value.clone());
                paths.truncate(RECENT_LIMIT);
            }
            self.dismiss_overlay(window, cx);
        } else {
            draft.creating = None;
            draft.status = Some(ui.message.clone());
        }
    }

    pub(super) fn creation_panel(&self, cx: &mut Context<Self>) -> Div {
        let draft = self.creation.as_ref().expect("workspace editor");
        let panel = div()
            .id("new-workspace")
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .flex()
            .flex_col()
            .w(ui_px(560.))
            .max_w_full()
            .max_h_full()
            .overflow_y_scroll()
            .gap(ui_px(8.))
            .p(ui_px(14.))
            .rounded_md()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .text_color(rgb(TEXT))
            .child(div().text_size(ui_px(18.)).child("New workspace"))
            .child(self.creation_hosts(draft, cx))
            .child(self.creation_folder(draft, cx))
            .child(self.creation_field(Field::Name, "Workspace name", &draft.name, cx))
            .when_some(draft.status.clone(), |panel, status| {
                panel.child(
                    div()
                        .text_size(ui_px(12.))
                        .text_color(rgb(MUTED))
                        .child(status),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(
                        button("cancel-workspace", "Cancel", false).on_click(cx.listener(
                            |view, _, window, cx| {
                                if view
                                    .creation
                                    .as_ref()
                                    .is_some_and(|draft| draft.creating.is_none())
                                {
                                    view.dismiss_overlay(window, cx);
                                }
                            },
                        )),
                    )
                    .child(
                        if draft.prepared.is_some() && draft.creating.is_none() && !draft.validating
                        {
                            button("create-workspace", "Create", true).on_click(cx.listener(
                                |view, _, window, cx| {
                                    view.submit_workspace(window, cx);
                                },
                            ))
                        } else {
                            button("create-workspace-disabled", "Create", false).opacity(0.5)
                        },
                    ),
            );
        self.overlay(panel, false, cx)
    }

    fn creation_hosts(&self, draft: &Draft, cx: &mut Context<Self>) -> Div {
        let hosts = self.host_choices(&draft.host.value);
        let start = draft.choice.saturating_sub(4);
        div()
            .flex()
            .flex_col()
            .gap(ui_px(4.))
            .child(
                div()
                    .text_color(rgb(MUTED))
                    .child(format!("Target · {}", machine_label(&draft.destination))),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(ui_px(5.))
                    .child(self.creation_field(
                        Field::Host,
                        "Search connections or enter SSH alias",
                        &draft.host,
                        cx,
                    ))
                    .child(
                        button("connect-workspace-alias", "Connect SSH", false).on_click(
                            cx.listener(|view, _, window, cx| {
                                view.connect_typed_host(window, cx);
                            }),
                        ),
                    ),
            )
            .when(draft.prepared.is_none() && !draft.connecting, |panel| {
                panel.child(
                    button("retry-workspace-host", "Connect to selected target", false).on_click(
                        cx.listener(|view, _, window, cx| {
                            view.prepare_folder_host(true, window, cx)
                        }),
                    ),
                )
            })
            .when(draft.field == Field::Host && !draft.connecting, |panel| {
                panel.children(hosts.into_iter().enumerate().skip(start).take(5).map(
                    |(index, target)| {
                        button(
                            format!("workspace-host-{index}"),
                            machine_label(&target),
                            index == draft.choice,
                        )
                        .on_click(cx.listener(
                            move |view, _, window, cx| view.choose_host(target.clone(), window, cx),
                        ))
                    },
                ))
            })
    }

    fn creation_folder(&self, draft: &Draft, cx: &mut Context<Self>) -> Div {
        let Some(prepared) = &draft.prepared else {
            return div();
        };
        if !prepared.supports_directory {
            return div().text_color(rgb(MUTED)).child("Update Corvid on this target to choose a folder. You can create a workspace with its usual starting location.");
        }
        let list = draft.listing.as_ref();
        let start = draft.choice.saturating_sub(4);
        div()
            .flex()
            .flex_col()
            .gap(ui_px(4.))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(ui_px(5.))
                    .child(self.creation_field(
                        Field::Folder,
                        "Folder · optional",
                        &draft.folder,
                        cx,
                    ))
                    .child(
                        button("browse-workspace", "Browse…", false).on_click(cx.listener(
                            |view, _, window, cx| {
                                let draft = view.creation.as_mut().expect("workspace editor");
                                draft.set_folder(if draft.folder.value.is_empty() {
                                    "~".into()
                                } else {
                                    draft.folder.value.clone()
                                });
                                window.focus(&view.setup_focus);
                                cx.notify();
                            },
                        )),
                    ),
            )
            .when(draft.expanded, |panel| {
                let parent = list.and_then(|list| list.parent.clone());
                panel
                    .child(
                        div()
                            .flex()
                            .gap(ui_px(4.))
                            .child(folder_button(
                                "workspace-home",
                                "Home",
                                "~".into(),
                                false,
                                cx,
                            ))
                            .when_some(parent, |row, parent| {
                                row.child(folder_button(
                                    "workspace-parent",
                                    "Up",
                                    parent,
                                    false,
                                    cx,
                                ))
                            })
                            .child(
                                button(
                                    "workspace-hidden",
                                    if draft.hidden {
                                        "Hide hidden"
                                    } else {
                                        "Show hidden"
                                    },
                                    false,
                                )
                                .on_click(cx.listener(
                                    |view, _, _, cx| {
                                        let draft =
                                            view.creation.as_mut().expect("workspace editor");
                                        draft.hidden = !draft.hidden;
                                        draft.query(false);
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(folder_button(
                                "workspace-default",
                                "Use default",
                                String::new(),
                                false,
                                cx,
                            )),
                    )
                    .when_some(list, |panel, list| {
                        panel
                            .child(
                                div()
                                    .truncate()
                                    .text_color(rgb(MUTED))
                                    .child(list.directory.clone()),
                            )
                            .children(list.entries.iter().enumerate().skip(start).take(5).map(
                                |(index, entry)| {
                                    folder_button(
                                        format!("workspace-folder-{index}"),
                                        entry.name.clone(),
                                        entry.path.clone(),
                                        index == draft.choice,
                                        cx,
                                    )
                                },
                            ))
                            .when(list.entries.len() > 5, |panel| {
                                let count = list.entries.len();
                                panel.child(
                                    div()
                                        .flex()
                                        .gap(ui_px(5.))
                                        .when(start > 0, |nav| {
                                            nav.child(
                                                button("folder-prev", "Previous folders", false)
                                                    .on_click(cx.listener(
                                                        move |view, _, _, cx| {
                                                            view.creation
                                                                .as_mut()
                                                                .expect("workspace editor")
                                                                .choice = start.saturating_sub(1);
                                                            cx.notify();
                                                        },
                                                    )),
                                            )
                                        })
                                        .when(start + 5 < count, |nav| {
                                            nav.child(
                                                button("folder-next", "More folders", false)
                                                    .on_click(cx.listener(
                                                        move |view, _, _, cx| {
                                                            view.creation
                                                                .as_mut()
                                                                .expect("workspace editor")
                                                                .choice =
                                                                (start + 9).min(count - 1);
                                                            cx.notify();
                                                        },
                                                    )),
                                            )
                                        }),
                                )
                            })
                            .when(list.truncated, |panel| {
                                panel.child(
                                    div().text_color(rgb(MUTED)).child(
                                        "Type more of the folder name to narrow the results.",
                                    ),
                                )
                            })
                    })
                    .when_some(self.recent_folders.get(&prepared.host), |panel, paths| {
                        panel
                            .child(div().text_color(rgb(MUTED)).child("Recent folders"))
                            .children(paths.iter().enumerate().take(3).map(|(index, path)| {
                                folder_button(
                                    format!("workspace-recent-{index}"),
                                    path.clone(),
                                    path.clone(),
                                    false,
                                    cx,
                                )
                            }))
                    })
            })
    }

    fn creation_field(
        &self,
        field: Field,
        label: &'static str,
        text: &Text,
        cx: &mut Context<Self>,
    ) -> Div {
        let draft = self.creation.as_ref().expect("workspace editor");
        let active = draft.field == field;
        let cursor = if active { "▏" } else { "" };
        let content = format!(
            "{}{cursor}{}",
            &text.value[..text.cursor],
            &text.value[text.cursor..]
        );
        let focus = self.setup_focus.clone();
        let entity = cx.entity();
        div()
            .flex()
            .flex_col()
            .gap(ui_px(3.))
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .text_size(ui_px(12.))
                    .text_color(rgb(MUTED))
                    .child(label),
            )
            .child(
                div()
                    .relative()
                    .h(ui_px(32.))
                    .px(ui_px(6.))
                    .py(ui_px(4.))
                    .overflow_hidden()
                    .bg(rgb(if active && !text.selection().is_empty() {
                        ACTIVE_FILL
                    } else {
                        BACKGROUND
                    }))
                    .border_1()
                    .border_color(rgb(if active { ACTIVE } else { BORDER }))
                    .child(content)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, window, cx| {
                            let draft = view.creation.as_mut().expect("workspace editor");
                            draft.field = field;
                            draft.choice = 0;
                            window.focus(&view.setup_focus);
                            cx.notify();
                        }),
                    )
                    .when(active, |field| {
                        field.child(
                            canvas(
                                |_, _, _| (),
                                move |bounds, _, window, cx| {
                                    window.handle_input(
                                        &focus,
                                        ElementInputHandler::new(bounds, entity.clone()),
                                        cx,
                                    );
                                },
                            )
                            .absolute()
                            .inset_0(),
                        )
                    }),
            )
    }
}

fn folder_button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    path: String,
    active: bool,
    cx: &mut Context<WorkspaceView>,
) -> Stateful<Div> {
    button(id, label, active)
        .truncate()
        .on_click(cx.listener(move |view, _, window, cx| {
            let draft = view.creation.as_mut().expect("workspace editor");
            if draft.creating.is_none() {
                draft.set_folder(path.clone());
                window.focus(&view.setup_focus);
                cx.notify();
            }
        }))
}
