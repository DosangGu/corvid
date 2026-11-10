use corvid_core::layout::Axis;
use uuid::Uuid;

#[derive(Clone, Copy, Default)]
pub(crate) struct Keymap {
    pub(crate) tabs: bool,
    pub(crate) pairing: bool,
    pub(crate) panes: bool,
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
    Detach,
    Tab(TabCommand),
    PickTab,
    PairController,
}

impl Keymap {
    pub(super) fn command(self, bytes: &[u8]) -> Option<KeyCommand> {
        match bytes {
            b"d" => Some(KeyCommand::Detach),
            b"n" if self.tabs => Some(KeyCommand::Tab(TabCommand::Next)),
            b"p" if self.tabs => Some(KeyCommand::Tab(TabCommand::Previous)),
            b"t" if self.tabs => Some(KeyCommand::Tab(TabCommand::Create)),
            b"%" if self.panes => Some(KeyCommand::Tab(TabCommand::Split(Axis::Horizontal))),
            b"\"" if self.panes => Some(KeyCommand::Tab(TabCommand::Split(Axis::Vertical))),
            b"o" if self.panes => Some(KeyCommand::Tab(TabCommand::NextPane)),
            [digit @ b'1'..=b'9'] if self.tabs => Some(KeyCommand::Tab(TabCommand::Index(
                usize::from(digit - b'1'),
            ))),
            b"w" if self.tabs => Some(KeyCommand::PickTab),
            b"r" if self.pairing => Some(KeyCommand::PairController),
            _ => None,
        }
    }
}
