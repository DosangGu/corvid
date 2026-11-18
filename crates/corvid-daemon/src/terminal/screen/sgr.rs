use std::fmt::Write as _;

#[derive(Default)]
pub(super) struct Normalizer(vte::Parser);

impl Normalizer {
    pub(super) fn process(&mut self, mut bytes: &[u8], mut process: impl FnMut(&[u8], bool)) {
        let mut sgr = Sgr::default();
        while !bytes.is_empty() {
            let consumed = self.0.advance_until_terminated(&mut sgr, bytes);
            let (chunk, rest) = bytes.split_at(consumed);
            if let Some(replacement) = sgr.replacement.take() {
                // Feed the prefix without the final 'm'. The replacement's ESC
                // cancels the pending CSI, even when it began in an earlier chunk.
                process(&chunk[..chunk.len() - 1], false);
                process(replacement.as_bytes(), false);
            } else {
                process(chunk, std::mem::take(&mut sgr.clear_history));
            }
            bytes = rest;
        }
    }
}

#[derive(Default)]
struct Sgr {
    replacement: Option<String>,
    clear_history: bool,
}

impl vte::Perform for Sgr {
    fn esc_dispatch(&mut self, intermediates: &[u8], _: bool, action: u8) {
        self.clear_history = intermediates.is_empty() && action == b'c';
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, c: char) {
        // Match vt100's ED/DECSED dispatch, including fragmented CSI.
        self.clear_history = c == 'J'
            && matches!(intermediates.first(), None | Some(b'?'))
            && params.iter().next().and_then(|group| group.first()) == Some(&3);
        if c != 'm' || ignore || !intermediates.is_empty() {
            return;
        }
        if !params
            .iter()
            .any(|group| rgb_without_color_space(group).is_some())
        {
            return;
        }

        // vt100 accepts compact colon RGB but does not accept the color space
        // subparameter. Preserve the grouping and order of every other parameter.
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

    fn terminated(&self) -> bool {
        self.replacement.is_some() || self.clear_history
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
