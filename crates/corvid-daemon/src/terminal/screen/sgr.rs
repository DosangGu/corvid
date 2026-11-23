use std::fmt::Write as _;

mod clipboard;
use clipboard::Clipboard;

#[derive(Default)]
pub(super) struct Normalizer {
    parser: vte::Parser,
    clipboard: Option<Clipboard>,
}

impl Normalizer {
    pub(super) fn process(&mut self, mut bytes: &[u8], mut process: impl FnMut(&[u8])) {
        let mut sgr = Sgr {
            capture_clipboard: self.clipboard.is_some(),
            ..Default::default()
        };
        while !bytes.is_empty() {
            let consumed = self.parser.advance_until_terminated(&mut sgr, bytes);
            let (chunk, rest) = bytes.split_at(consumed);
            if let Some(clipboard) = &mut self.clipboard {
                clipboard.advance(chunk, std::mem::take(&mut sgr.clipboard_dispatched));
            }
            if let Some(replacement) = sgr.replacement.take() {
                // Feed the prefix without the final 'm'. The replacement's ESC
                // cancels the pending CSI, even when it began in an earlier chunk.
                process(&chunk[..chunk.len() - 1]);
                process(replacement.as_bytes());
            } else {
                process(chunk);
            }
            bytes = rest;
        }
    }

    pub(super) fn configure_clipboard(&mut self, enabled: bool, reset: bool) {
        if !enabled {
            self.clipboard = None;
        } else if reset || self.clipboard.is_none() {
            self.clipboard = Some(Clipboard::default());
        }
    }

    pub(super) fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.as_mut()?.take()
    }
}

#[derive(Default)]
struct Sgr {
    replacement: Option<String>,
    capture_clipboard: bool,
    clipboard_dispatched: bool,
}

impl vte::Perform for Sgr {
    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, c: char) {
        if c != 'm' || ignore || !intermediates.is_empty() {
            return;
        }
        if !params
            .iter()
            .any(|group| rgb_without_color_space(group).is_some())
        {
            return;
        }

        // Accept ISO colon RGB with an omitted/default color space. Preserve
        // the grouping and order of the other rendition parameters.
        let mut sgr = String::from("\x1b[");
        for (index, group) in params.iter().enumerate() {
            if index != 0 {
                sgr.push(';');
            }
            let rgb = rgb_without_color_space(group);
            let group = rgb.as_ref().map_or(group, |rgb| rgb.as_slice());
            for (index, value) in group.iter().enumerate() {
                if index != 0 {
                    sgr.push(':');
                }
                write!(sgr, "{value}").unwrap();
            }
        }
        sgr.push('m');
        self.replacement = Some(sgr);
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
        self.clipboard_dispatched =
            self.capture_clipboard && params.first() == Some(&b"52".as_slice());
    }

    fn terminated(&self) -> bool {
        self.replacement.is_some() || self.clipboard_dispatched
    }
}

fn rgb_without_color_space(group: &[u16]) -> Option<[u16; 5]> {
    let &[
        channel @ (38 | 48),
        2,
        0,
        r @ 0..=255,
        g @ 0..=255,
        b @ 0..=255,
    ] = group
    else {
        return None;
    };
    Some([channel, 2, r, g, b])
}
