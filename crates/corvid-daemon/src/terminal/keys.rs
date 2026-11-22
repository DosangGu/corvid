use corvid_core::layout::{Axis, CloseTarget, Location};
use uuid::Uuid;

#[derive(Clone, Copy, Default)]
pub(crate) struct Keymap {
    pub(crate) workspace_ui: bool,
    pub(crate) naming: bool,
    pub(crate) tabs: bool,
    pub(crate) pairing: bool,
    pub(crate) panes: bool,
    pub(crate) close: bool,
    pub(crate) pane_writer: bool,
    pub(crate) disconnect: bool,
    pub(crate) clipboard_write: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputMode {
    Passthrough,
    Command,
}

#[derive(Clone, Copy)]
pub(crate) enum TabCommand {
    Next,
    Previous,
    Create,
    Split(Axis),
    NextPane,
    Pane(Uuid),
    Workspace(Uuid),
    SplitAt(Uuid, Axis),
    Index(usize),
    Select(Uuid),
}

pub(super) enum KeyCommand {
    BeginRename,
    RenameTab(String),
    Detach,
    Tab(TabCommand),
    Close(CloseKind),
    PickTab,
    Browse(super::navigation::Kind),
    End(CloseTarget),
    Click(Uuid, super::navigation::Mouse),
    PairController,
    DisconnectOthers,
}

pub(super) enum CloseKind {
    Pane,
    Tab,
    Workspace,
}

impl CloseKind {
    pub(super) fn target(self, location: Location) -> CloseTarget {
        match self {
            Self::Pane => CloseTarget::Pane(location),
            Self::Tab => CloseTarget::Tab {
                workspace: location.workspace,
                tab: location.tab,
            },
            Self::Workspace => CloseTarget::Workspace(location.workspace),
        }
    }
}

impl Keymap {
    pub(super) fn command(self, bytes: &[u8]) -> Option<KeyCommand> {
        if self.workspace_ui {
            use super::navigation::Kind;
            use corvid_core::navigation::WorkspaceCommand as Cmd;
            if let [key] = bytes
                && let Some(command) = Cmd::from_key(char::from(*key))
            {
                return match command {
                    Cmd::Workspaces => Some(KeyCommand::Browse(Kind::Workspaces)),
                    Cmd::Tabs if self.tabs => Some(KeyCommand::Browse(Kind::Tabs)),
                    Cmd::Panes if self.panes => Some(KeyCommand::Browse(Kind::Panes)),
                    Cmd::PreviousTab if self.tabs => Some(KeyCommand::Tab(TabCommand::Previous)),
                    Cmd::NextTab if self.tabs => Some(KeyCommand::Tab(TabCommand::Next)),
                    Cmd::RenameTab if self.naming => Some(KeyCommand::BeginRename),
                    Cmd::Detach => Some(KeyCommand::Detach),
                    _ => None,
                };
            }
            // Workspace navigation handles closure through the visible selectors.
            if matches!(bytes, b"X" | b"T" | b"S") {
                return None;
            }
        }
        match bytes {
            b"." if self.naming => Some(KeyCommand::BeginRename),
            b"d" => Some(KeyCommand::Detach),
            b"n" if self.tabs => Some(KeyCommand::Tab(TabCommand::Next)),
            b"p" if self.tabs => Some(KeyCommand::Tab(TabCommand::Previous)),
            b"t" if self.tabs => Some(KeyCommand::Tab(TabCommand::Create)),
            b"%" if self.panes => Some(KeyCommand::Tab(TabCommand::Split(Axis::Horizontal))),
            b"\"" if self.panes => Some(KeyCommand::Tab(TabCommand::Split(Axis::Vertical))),
            b"o" if self.panes => Some(KeyCommand::Tab(TabCommand::NextPane)),
            b"X" if self.close => Some(KeyCommand::Close(CloseKind::Pane)),
            b"T" if self.close => Some(KeyCommand::Close(CloseKind::Tab)),
            b"S" if self.close => Some(KeyCommand::Close(CloseKind::Workspace)),
            [digit @ b'1'..=b'9'] if self.tabs => Some(KeyCommand::Tab(TabCommand::Index(
                usize::from(digit - b'1'),
            ))),
            b"w" if self.tabs => Some(KeyCommand::PickTab),
            b"r" if self.pairing => Some(KeyCommand::PairController),
            b"D" if self.disconnect => Some(KeyCommand::DisconnectOthers),
            _ => None,
        }
    }
}
