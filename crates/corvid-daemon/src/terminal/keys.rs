use corvid_core::layout::{Axis, CloseTarget, Location};
use uuid::Uuid;

#[derive(Clone, Copy, Default)]
pub(crate) struct Keymap {
    pub(crate) naming: bool,
    pub(crate) tabs: bool,
    pub(crate) pairing: bool,
    pub(crate) panes: bool,
    pub(crate) close: bool,
    pub(crate) pane_writer: bool,
    pub(crate) disconnect: bool,
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
    Index(usize),
    Select(Uuid),
}

pub(super) enum KeyCommand {
    BeginRename(bool),
    Rename(bool, String),
    Detach,
    Tab(TabCommand),
    Close(CloseKind),
    PickTab,
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
        match bytes {
            b"." if self.naming => Some(KeyCommand::BeginRename(true)),
            b"," if self.naming => Some(KeyCommand::BeginRename(false)),
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
