pub const COLUMNS: usize = 5;
pub const KEY_COUNT: usize = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Amber,
    Blue,
    Green,
    Violet,
    Rose,
    Slate,
}

impl Tone {
    pub fn colors(self) -> (u32, u32) {
        match self {
            Self::Amber => (0xf7b968, 0x322720),
            Self::Blue => (0x92b8ff, 0x222c42),
            Self::Green => (0x83dec0, 0x20332f),
            Self::Violet => (0xc8afff, 0x2d263e),
            Self::Rose => (0xf4a3bb, 0x382630),
            Self::Slate => (0xc3cedd, 0x292f38),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behavior {
    Momentary,
    Toggle,
    NextPage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub label: &'static str,
    pub glyph: &'static str,
    pub tone: Tone,
    pub behavior: Behavior,
}

impl Key {
    const fn press(label: &'static str, glyph: &'static str, tone: Tone) -> Self {
        Self {
            label,
            glyph,
            tone,
            behavior: Behavior::Momentary,
        }
    }

    const fn toggle(label: &'static str, glyph: &'static str, tone: Tone) -> Self {
        Self {
            behavior: Behavior::Toggle,
            ..Self::press(label, glyph, tone)
        }
    }

    const fn next_page(label: &'static str) -> Self {
        Self {
            behavior: Behavior::NextPage,
            ..Self::press(label, "page", Blue)
        }
    }
}

pub struct Page {
    pub name: &'static str,
    pub glyph: &'static str,
    pub keys: [Key; KEY_COUNT],
}

use Tone::{Amber, Blue, Green, Rose, Slate, Violet};

pub const PAGES: [Page; 2] = [
    Page {
        name: "Workspace",
        glyph: "grid",
        keys: [
            Key::toggle("Focus", "focus", Amber),
            Key::press("Capture", "capture", Blue),
            Key::press("Notes", "notes", Violet),
            Key::press("Tasks", "tasks", Green),
            Key::press("Timer", "timer", Amber),
            Key::toggle("Mic", "mic", Rose),
            Key::toggle("Camera", "camera", Violet),
            Key::press("Volume −", "volume-down", Blue),
            Key::press("Volume +", "volume-up", Blue),
            Key::toggle("Mute", "mute", Rose),
            Key::press("Previous", "previous", Slate),
            Key::toggle("Play / Pause", "play", Green),
            Key::press("Next", "next", Slate),
            Key::toggle("Break", "coffee", Amber),
            Key::next_page("System"),
        ],
    },
    Page {
        name: "System",
        glyph: "settings",
        keys: [
            Key::press("Lock", "lock", Blue),
            Key::press("Sleep", "moon", Violet),
            Key::press("Dim", "dim", Amber),
            Key::press("Brighten", "sun", Amber),
            Key::press("Settings", "settings", Blue),
            Key::toggle("Alerts", "bell", Rose),
            Key::toggle("Airplane", "plane", Violet),
            Key::toggle("Bluetooth", "bluetooth", Blue),
            Key::toggle("Wi-Fi", "wifi", Green),
            Key::toggle("Power Save", "battery", Green),
            Key::press("Copy", "copy", Slate),
            Key::press("Paste", "paste", Blue),
            Key::press("Undo", "undo", Violet),
            Key::press("Redo", "redo", Violet),
            Key::next_page("Workspace"),
        ],
    },
];
