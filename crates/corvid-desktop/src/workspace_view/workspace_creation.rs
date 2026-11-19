use std::ops::Range;

use corvid_protocol::wire::{
    BrowseDirectories, DirectoryEntry, DirectoryListing, WorkspaceCreation,
};
use gpui::{ElementInputHandler, Keystroke, MouseButton, ScrollHandle, UTF16Selection, canvas};
use tokio::sync::{mpsc, watch};

use super::switcher::machine_label;
use super::*;
use crate::daemon_prompt::{self, Prompt, Prompts};

const MAX_PATH: usize = 4096;
const RECENT_LIMIT: usize = 5;

mod browser;

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Folder,
    Folders,
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

enum FolderQuery {
    Browse { reselect: Option<String> },
    Complete,
    Validate,
}

pub(super) struct Draft {
    destination: Destination,
    return_to_picker: bool,
    folder: Text,
    name: Text,
    field: Field,
    default_name: String,
    name_edited: bool,
    hidden: bool,
    prepared: Option<Prepared>,
    browser: Option<Task<()>>,
    queries: Option<watch::Sender<(u64, BrowseDirectories)>>,
    revision: u64,
    pending: Option<FolderQuery>,
    listing: Option<DirectoryListing>,
    choice: usize,
    scroll: ScrollHandle,
    reveal_folder: bool,
    status: Option<String>,
    connecting: bool,
    creating: Option<Option<String>>,
}

impl Draft {
    fn new(destination: Destination, return_to_picker: bool, default_name: String) -> Self {
        let mut name = Text::default();
        name.set(default_name.clone());
        Self {
            destination,
            return_to_picker,
            folder: Text::default(),
            name,
            field: Field::Folders,
            default_name,
            name_edited: false,
            hidden: false,
            prepared: None,
            browser: None,
            queries: None,
            revision: 0,
            pending: None,
            listing: None,
            choice: 0,
            scroll: ScrollHandle::new(),
            reveal_folder: false,
            status: None,
            connecting: false,
            creating: None,
        }
    }

    pub(super) fn text(&self) -> &Text {
        match self.field {
            Field::Folder | Field::Folders => &self.folder,
            Field::Name => &self.name,
        }
    }

    fn text_mut(&mut self) -> &mut Text {
        match self.field {
            Field::Folder | Field::Folders => &mut self.folder,
            Field::Name => &mut self.name,
        }
    }

    fn propose_name(&mut self, name: Option<String>) {
        if !self.name_edited {
            self.name
                .set(name.unwrap_or_else(|| self.default_name.clone()));
        }
    }

    fn query(&mut self, path: String, kind: FolderQuery) {
        let Some(queries) = &self.queries else {
            return;
        };
        self.revision += 1;
        queries.send_replace((
            self.revision,
            BrowseDirectories {
                path,
                complete: matches!(kind, FolderQuery::Complete),
                show_hidden: self.hidden,
                validate_only: matches!(kind, FolderQuery::Validate),
            },
        ));
        self.pending = Some(kind);
        self.status = None;
    }

    fn browse_folder(&mut self, path: String, reselect: Option<String>) {
        if self.creating.is_none() && self.pending.is_none() && self.text().marked.is_none() {
            self.field = Field::Folders;
            self.query(path, FolderQuery::Browse { reselect });
        }
    }

    fn selected_folder(&self) -> Option<&DirectoryEntry> {
        self.listing.as_ref()?.entries.get(self.choice)
    }

    fn select_folder(&mut self, index: usize) {
        let count = self.listing.as_ref().map_or(0, |list| list.entries.len());
        self.choice = index.min(count.saturating_sub(1));
        if count != 0 {
            self.scroll.scroll_to_item(self.choice);
        }
    }

    fn refresh_folders(&mut self) {
        if let Some(list) = &self.listing
            && self.folder.value == list.directory
        {
            let reselect = self.selected_folder().map(|entry| entry.path.clone());
            self.browse_folder(list.directory.clone(), reselect);
        } else {
            self.query(self.folder.value.clone(), FolderQuery::Complete);
        }
    }

    fn edited(&mut self) {
        match self.field {
            Field::Folder => {
                self.revision += 1;
                self.pending = None;
                self.listing = None;
                self.status = None;
                if self.folder.marked.is_none() {
                    self.query(self.folder.value.clone(), FolderQuery::Complete);
                }
            }
            Field::Folders => {}
            Field::Name => self.name_edited = true,
        }
    }

    fn can_submit(&self) -> bool {
        self.prepared.is_some()
            && self.creating.is_none()
            && matches!(self.pending, None | Some(FolderQuery::Complete))
            && self.folder.marked.is_none()
            && self.name.marked.is_none()
    }

    fn folder_key(&mut self, key: &Keystroke) -> bool {
        if !self
            .prepared
            .as_ref()
            .is_some_and(|host| host.supports_directory)
            || key.modifiers.control
            || key.modifiers.alt
            || key.modifiers.platform
        {
            return false;
        }
        match (self.field, key.key.as_str(), key.modifiers.shift) {
            (Field::Folders, "tab", true) | (Field::Name, "tab", false) => {
                self.field = Field::Folder;
            }
            (Field::Folder, "tab", true) => self.field = Field::Name,
            (Field::Name, "tab", true) => self.field = Field::Folders,
            (Field::Folder | Field::Folders, "tab", false) => {
                if self.pending.is_none() {
                    if let Some(entry) = self.selected_folder() {
                        self.browse_folder(entry.path.clone(), None);
                    } else {
                        self.field = Field::Name;
                    }
                }
            }
            (Field::Folder, "up" | "down", false) => {
                self.field = Field::Folders;
                self.select_folder(self.choice);
            }
            (Field::Folders, "up", false) => self.select_folder(self.choice.saturating_sub(1)),
            (Field::Folders, "down", false) => self.select_folder(self.choice + 1),
            (Field::Folders, "right", false) => {
                if let Some(entry) = self.selected_folder() {
                    self.browse_folder(entry.path.clone(), None);
                }
            }
            (Field::Folders, "left", false) => {
                if let Some(list) = &self.listing
                    && let Some(parent) = &list.parent
                {
                    self.browse_folder(parent.clone(), Some(list.directory.clone()));
                }
            }
            _ => return false,
        }
        true
    }

    fn edit_key(
        &mut self,
        key: &Keystroke,
        command: bool,
        cx: &mut Context<WorkspaceView>,
    ) -> bool {
        if self.field == Field::Folders {
            return false;
        }
        let edit = self.text_mut();
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
            _ => return false,
        }
        if changed {
            self.edited();
        }
        true
    }
}

impl WorkspaceView {
    pub(super) fn new_workspace(
        &mut self,
        destination: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ui = self.session.as_ref().map(Session::ui);
        if self.connecting(ui.as_ref()) {
            return;
        }
        let count = ui
            .filter(|_| self.destination.as_ref() == Some(&destination))
            .and_then(|ui| ui.inventory)
            .or_else(|| {
                self.known_sessions
                    .get(&destination)
                    .and_then(|known| known.inventory.clone())
            })
            .map_or(0, |inventory| inventory.workspaces.len());
        let return_to_picker = std::mem::take(&mut self.switcher_open);
        self.menu_open = false;
        self.naming = None;
        self.creation = Some(Draft::new(
            destination,
            return_to_picker,
            format!("Workspace {}", count + 1),
        ));
        window.focus(&self.setup_focus);
        self.prepare_folder_host(window, cx);
    }

    pub(super) fn cancel_creation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.creation.as_ref().expect("workspace editor");
        if draft.creating.is_some() {
            return;
        }
        if draft.return_to_picker {
            self.creation = None;
            self.switcher_open = true;
            window.focus(&self.setup_focus);
            cx.notify();
        } else {
            self.dismiss_overlay(window, cx);
        }
    }

    fn prepare_folder_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.creation.as_mut().expect("workspace editor");
        let destination = draft.destination.clone();
        let current = self
            .session
            .as_ref()
            .map(Session::ui)
            .filter(|ui| ui.connected && self.destination.as_ref() == Some(&destination));
        let known_endpoint = current.as_ref().and_then(|ui| ui.endpoint.clone());
        let expected_host = current.and_then(|ui| ui.host_id).or_else(|| {
            self.known_sessions
                .get(&destination)
                .map(|known| known.host_id.clone())
        });
        draft.prepared = None;
        draft.browser = None;
        draft.queries = None;
        draft.pending = None;
        draft.listing = None;
        draft.connecting = true;
        draft.status = Some(format!("Connecting to {}…", machine_label(&destination)));
        let result = browser::start(
            destination,
            self.agent_deck.state.clone(),
            known_endpoint,
            expected_host,
            self.store.clone(),
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
            && draft.field != Field::Folders
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
        if draft.text().marked.is_some() {
            return;
        }
        let command = key.modifiers.control || key.modifiers.platform;
        match key.key.as_str() {
            "enter" if !event.is_held => self.submit_workspace(window, cx),
            "enter" => {}
            "escape" => self.cancel_creation(window, cx),
            _ => {
                let draft = self.creation.as_mut().expect("workspace editor");
                if !draft.folder_key(key) && !draft.edit_key(key, command, cx) {
                    return;
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn submit_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ui = self.session.as_ref().map(Session::ui);
        if self.connecting(ui.as_ref()) {
            return;
        }
        let draft = self.creation.as_mut().expect("workspace editor");
        if !draft.can_submit() {
            return;
        }
        if !draft.folder.value.is_empty() {
            draft.query(draft.folder.value.clone(), FolderQuery::Validate);
            draft.status = Some("Checking the workspace folder…".into());
            cx.notify();
            return;
        }
        draft.propose_name(None);
        self.finish_workspace(window, cx);
    }

    fn finish_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ui = self.session.as_ref().map(Session::ui);
        let busy = self.connecting(ui.as_ref());
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
        if busy {
            draft.status = Some("Wait for the current connection to finish and try again.".into());
            cx.notify();
            return;
        }
        let old = self
            .selected
            .as_ref()
            .filter(|_| self.destination.as_ref() == Some(&draft.destination))
            .map(|view| view.workspace_id.clone());
        draft.creating = Some(old);
        draft.status = Some("Creating workspace…".into());
        if self.destination.as_ref() == Some(&draft.destination)
            && ui.is_some_and(|ui| {
                ui.connected && !ui.changing && ui.host_id.as_ref() == Some(&prepared.host)
            })
            && let Some(session) = &self.session
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

    pub(super) fn creation_failed(&mut self, error: String) {
        if let Some(draft) = &mut self.creation
            && draft.creating.take().is_some()
        {
            draft.status = Some(error);
        }
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
            && ui.host_id.as_ref() == draft.prepared.as_ref().map(|prepared| &prepared.host)
            && self.destination.as_ref() == Some(&draft.destination)
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
            self.creation_failed(ui.message.clone());
        }
    }

    pub(super) fn creation_panel(&mut self, window: &Window, cx: &mut Context<Self>) -> Div {
        let draft = self.creation.as_mut().expect("workspace editor");
        if std::mem::take(&mut draft.reveal_folder) {
            let revision = draft.revision;
            let choice = draft.choice;
            let view = cx.entity().downgrade();
            // GPUI uses the scroll viewport bounds from the previous layout pass.
            window.on_next_frame(move |_, cx| {
                let _ = view.update(cx, |view, cx| {
                    if let Some(draft) = &view.creation
                        && draft.revision == revision
                        && draft.choice == choice
                    {
                        if choice == 0 {
                            draft.scroll.scroll_to_top_of_item(0);
                        } else {
                            draft.scroll.scroll_to_item(choice);
                        }
                        cx.notify();
                    }
                });
            });
        }
        let draft = self.creation.as_ref().expect("workspace editor");
        let offset = 54.;
        let status = draft.status.as_deref().or(match draft.pending {
            Some(FolderQuery::Browse { .. } | FolderQuery::Complete) => Some("Loading folders…"),
            _ => None,
        });
        let panel = div()
            .id("new-workspace")
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .flex()
            .flex_col()
            .w(ui_px(560.))
            .max_w_full()
            .max_h(window.viewport_size().height - ui_px(offset).to_pixels(window.rem_size()))
            .gap(ui_px(8.))
            .p(ui_px(14.))
            .rounded_md()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .flex_none()
                    .text_size(ui_px(18.))
                    .truncate()
                    .child(format!("New workspace on {}", machine_label(&draft.destination))),
            )
            .when_some(draft.prepared.as_ref(), |panel, prepared| {
                if prepared.supports_directory {
                    panel.child(self.creation_folders(draft, prepared, cx))
                } else {
                    panel.child(
                        div().text_color(rgb(MUTED)).child(
                            "Update Corvid on this machine to choose a folder. The default starting folder is available.",
                        ),
                    )
                }
                .child(self.creation_field(Field::Name, "Workspace name", &draft.name, cx))
            })
            .when_some(status, |panel, status| {
                panel.child(
                    div()
                        .flex_none()
                        .text_size(ui_px(12.))
                        .text_color(rgb(MUTED))
                        .child(status.to_owned()),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .justify_between()
                    .child(
                        button("cancel-workspace", "Cancel", false).on_click(cx.listener(
                            |view, _, window, cx| view.cancel_creation(window, cx),
                        )),
                    )
                    .child(if draft.can_submit() {
                        button("create-workspace", "Create", true).on_click(cx.listener(
                            |view, _, window, cx| view.submit_workspace(window, cx),
                        ))
                    } else if draft.prepared.is_none() && !draft.connecting {
                        button("retry-workspace-host", "Retry", true).on_click(cx.listener(
                            |view, _, window, cx| view.prepare_folder_host(window, cx),
                        ))
                    } else {
                        button("create-workspace-disabled", "Create", false).opacity(0.5)
                    }),
            );
        self.overlay(panel, false, cx)
    }

    fn creation_folders(&self, draft: &Draft, prepared: &Prepared, cx: &mut Context<Self>) -> Div {
        let parent = draft
            .listing
            .as_ref()
            .and_then(|list| Some((list.parent.clone()?, list.directory.clone())));
        div()
            .flex()
            .flex_col()
            .flex_shrink()
            .min_h_0()
            .gap(ui_px(4.))
            .child(self.creation_field(Field::Folder, "Folder path", &draft.folder, cx))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .flex_wrap()
                    .gap(ui_px(4.))
                    .child(folder_button(
                        "workspace-home",
                        "Home",
                        "~".into(),
                        false,
                        cx,
                    ))
                    .when_some(parent, |row, (parent, child)| {
                        row.child(
                            button("workspace-parent", "Up", false).on_click(cx.listener(
                                move |view, _, window, cx| {
                                    let draft = view.creation.as_mut().expect("workspace editor");
                                    draft.browse_folder(parent.clone(), Some(child.clone()));
                                    window.focus(&view.setup_focus);
                                    cx.notify();
                                },
                            )),
                        )
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
                        .on_click(cx.listener(|view, _, _, cx| {
                            let draft = view.creation.as_mut().expect("workspace editor");
                            if draft.pending.is_none()
                                && draft.creating.is_none()
                                && draft.text().marked.is_none()
                            {
                                draft.hidden = !draft.hidden;
                                draft.refresh_folders();
                                cx.notify();
                            }
                        })),
                    )
                    .child(button("workspace-default", "Use default", false).on_click(
                        cx.listener(|view, _, window, cx| {
                            let draft = view.creation.as_mut().expect("workspace editor");
                            if draft.creating.is_none() {
                                draft.field = Field::Name;
                                draft.folder.set(String::new());
                                draft.revision += 1;
                                draft.pending = None;
                                draft.listing = None;
                                draft.status = None;
                                draft.propose_name(None);
                                window.focus(&view.setup_focus);
                                cx.notify();
                            }
                        }),
                    )),
            )
            .when_some(draft.listing.as_ref(), |panel, list| {
                panel
                    .child(
                        div()
                            .flex_none()
                            .text_size(ui_px(12.))
                            .text_color(rgb(MUTED))
                            .child("↑↓ Move   ←→ Browse   Tab Complete   Shift+Tab Edit path"),
                    )
                    .child(
                        div()
                            .id("workspace-folders")
                            .flex()
                            .flex_col()
                            .flex_shrink()
                            .min_h_0()
                            .max_h(ui_px(320.))
                            .overflow_y_scroll()
                            .track_scroll(&draft.scroll)
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(if draft.field == Field::Folders {
                                ACTIVE
                            } else {
                                BORDER
                            }))
                            .children(list.entries.iter().enumerate().map(|(index, entry)| {
                                folder_button(
                                    format!("workspace-folder-{index}"),
                                    entry.name.clone(),
                                    entry.path.clone(),
                                    index == draft.choice,
                                    cx,
                                )
                            }))
                            .when(list.entries.is_empty(), |list| {
                                list.child(
                                    div()
                                        .px(ui_px(10.))
                                        .py(ui_px(5.))
                                        .text_color(rgb(MUTED))
                                        .child("No matching folders"),
                                )
                            }),
                    )
                    .when(list.truncated, |panel| {
                        panel.child(
                            div()
                                .flex_none()
                                .text_size(ui_px(12.))
                                .text_color(rgb(MUTED))
                                .child("Type more of the folder name to narrow the results."),
                        )
                    })
            })
            .when_some(self.recent_folders.get(&prepared.host), |panel, paths| {
                panel.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(ui_px(4.))
                        .overflow_hidden()
                        .child(div().text_color(rgb(MUTED)).child("Recent"))
                        .children(paths.iter().enumerate().take(3).map(|(index, path)| {
                            folder_button(
                                format!("workspace-recent-{index}"),
                                path.clone(),
                                path.clone(),
                                false,
                                cx,
                            )
                            .flex_1()
                            .min_w_0()
                        })),
                )
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
            .flex_none()
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
                            if draft.creating.is_none() && draft.text().marked.is_none() {
                                draft.field = field;
                                window.focus(&view.setup_focus);
                                cx.notify();
                            }
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
            draft.browse_folder(path.clone(), None);
            window.focus(&view.setup_focus);
            cx.notify();
        }))
}

#[cfg(test)]
mod tests {
    use super::browser::Update;
    use super::*;

    fn connected_browser() -> (Draft, watch::Receiver<(u64, BrowseDirectories)>) {
        let (queries, pending) = watch::channel((0, BrowseDirectories::default()));
        let alias = SshAlias::new("test-machine".into()).unwrap();
        let mut draft = Draft::new(Destination::Ssh(alias.clone()), true, "Workspace 1".into());
        draft.queries = Some(queries);
        assert!(!draft.update(Update::Ready(Prepared {
            endpoint: ControlEndpoint::Ssh(alias),
            host: "test-host".into(),
            supports_directory: true,
        })));
        (draft, pending)
    }

    fn listing(directory: &str, parent: Option<&str>, names: &[&str]) -> DirectoryListing {
        DirectoryListing {
            directory: directory.into(),
            parent: parent.map(str::to_owned),
            folder_name: directory
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            entries: names
                .iter()
                .map(|name| DirectoryEntry {
                    name: (*name).into(),
                    path: format!("{}/{name}", directory.trim_end_matches('/')),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn press(draft: &mut Draft, key: &str) {
        assert!(draft.folder_key(&Keystroke::parse(key).unwrap()));
    }

    #[test]
    fn opens_home_then_reselects_the_folder_after_going_up() {
        let (mut draft, queries) = connected_browser();
        assert_eq!(queries.borrow().1.path, "~");
        let names: Vec<_> = (1..=32).map(|i| format!("folder-{i:02}")).collect();
        let names: Vec<_> = names.iter().map(String::as_str).collect();
        let home = listing("/home", Some("/"), &names);
        draft.update(Update::Listed(draft.revision, Ok(home.clone())));
        assert_eq!(draft.folder.value, "/home");
        for _ in 0..40 {
            press(&mut draft, "down");
        }
        assert_eq!(draft.choice, 31);
        press(&mut draft, "right");
        assert_eq!(queries.borrow().1.path, "/home/folder-32");
        draft.update(Update::Listed(
            draft.revision,
            Ok(listing("/home/folder-32", Some("/home"), &[])),
        ));
        assert_eq!(draft.folder.value, "/home/folder-32");
        assert_eq!(draft.name.value, "folder-32");
        press(&mut draft, "left");
        assert_eq!(queries.borrow().1.path, "/home");
        draft.update(Update::Listed(draft.revision, Ok(home)));
        assert_eq!(draft.selected_folder().unwrap().name, "folder-32");
    }

    #[test]
    fn tab_completes_a_path_and_ignores_older_replies() {
        let (mut draft, queries) = connected_browser();
        let old_revision = draft.revision;
        draft.update(Update::Listed(
            old_revision,
            Ok(listing("/home", Some("/"), &[])),
        ));
        press(&mut draft, "shift-tab");
        assert!(draft.field == Field::Folder);
        assert!(!draft.folder_key(&Keystroke::parse("left").unwrap()));
        draft.folder.set("/p".into());
        draft.edited();
        assert!(queries.borrow().1.complete);
        assert_eq!(queries.borrow().1.path, "/p");
        draft.update(Update::Listed(
            old_revision,
            Ok(listing("/home", Some("/"), &[])),
        ));
        assert!(draft.listing.is_none());
        draft.update(Update::Listed(
            draft.revision,
            Ok(listing("/", None, &["projects"])),
        ));
        assert_eq!(draft.folder.value, "/p");
        press(&mut draft, "tab");
        assert_eq!(queries.borrow().1.path, "/projects");
        assert!(!queries.borrow().1.complete);
        draft.name.set("My workspace".into());
        draft.name_edited = true;
        draft.update(Update::Listed(
            draft.revision,
            Ok(listing("/projects", Some("/"), &[])),
        ));
        assert_eq!(draft.folder.value, "/projects");
        assert_eq!(draft.name.value, "My workspace");
    }

    #[test]
    fn an_inaccessible_child_keeps_the_current_folder_and_root_has_no_parent() {
        let (mut draft, _) = connected_browser();
        draft.update(Update::Listed(
            draft.revision,
            Ok(listing("/", None, &["private"])),
        ));
        let at_root = draft.revision;
        press(&mut draft, "left");
        assert_eq!(draft.revision, at_root);
        press(&mut draft, "right");
        assert!(!draft.can_submit());
        draft.update(Update::Listed(
            draft.revision,
            Err(anyhow::anyhow!("Access denied")),
        ));
        assert_eq!(draft.folder.value, "/");
        assert_eq!(draft.selected_folder().unwrap().name, "private");
        assert_eq!(draft.status.as_deref(), Some("Access denied"));
        assert!(draft.can_submit());
    }
}
