use gpui::{Rgba, Window, rgb, rgba};

#[derive(Clone, Copy)]
pub(super) struct Colors {
    pub background: Rgba,
    pub panel: Rgba,
    pub bar: Rgba,
    pub empty_card: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub border: Rgba,
    pub accent: Rgba,
    pub hover: Rgba,
    pub selection: Rgba,
    pub selection_hover: Rgba,
    pub scrim: Rgba,
    pub working: Rgba,
    pub attention: Rgba,
    pub response: Rgba,
    pub error: Rgba,
}

impl Colors {
    pub fn for_window(window: &Window) -> Self {
        #[cfg(target_os = "macos")]
        if let Some(
            [
                panel,
                text,
                muted,
                border,
                accent,
                hover,
                selection,
                empty_card,
                dark,
            ],
        ) = crate::shell::macos_colors(window)
        {
            let (working, attention, response, error) = if dark != 0 {
                (0x80bfff, 0xffc469, 0x76dab5, 0xff9a9e)
            } else {
                (0x125ca6, 0x8a4d00, 0x09674c, 0xb22435)
            };
            return Self {
                background: rgba(0),
                panel: rgba(panel),
                bar: rgba(0),
                empty_card: rgba(empty_card),
                text: rgba(text),
                muted: rgba(muted),
                border: rgba(border),
                accent: rgba(accent),
                hover: rgba(hover),
                selection: rgba(selection),
                selection_hover: rgba(selection),
                scrim: rgba(0x00000055),
                working: rgb(working),
                attention: rgb(attention),
                response: rgb(response),
                error: rgb(error),
            };
        }

        let _ = window;
        Self {
            background: rgb(0x111113),
            panel: rgb(0x18181b),
            bar: rgb(0x18181b),
            empty_card: rgb(0x141416),
            text: rgb(0xededf0),
            muted: rgb(0x8b8b95),
            border: rgb(0x27272c),
            accent: rgb(0xe4e4e7),
            hover: rgb(0x222226),
            selection: rgb(0x2a2a30),
            selection_hover: rgb(0x34343c),
            scrim: rgba(0x00000088),
            working: rgb(0x8bb8e8),
            attention: rgb(0xe5a93c),
            response: rgb(0x6bc48d),
            error: rgb(0xe5737d),
        }
    }
}
