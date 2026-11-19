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
            background: rgb(0x0f1319),
            panel: rgb(0x151b23),
            bar: rgb(0x151b23),
            empty_card: rgb(0x0f1319),
            text: rgb(0xe5edf1),
            muted: rgb(0x8594a3),
            border: rgb(0x303b48),
            accent: rgb(0x77d8b7),
            hover: rgb(0x263541),
            selection: rgb(0x234238),
            selection_hover: rgb(0x31574a),
            scrim: rgba(0x00000088),
            working: rgb(0x78bdee),
            attention: rgb(0xf4c065),
            response: rgb(0x77d8b7),
            error: rgb(0xf18590),
        }
    }
}
