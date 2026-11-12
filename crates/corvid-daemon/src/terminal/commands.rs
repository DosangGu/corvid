use std::fmt::Write;

use corvid_core::Viewport;

use super::keys::Keymap;
use super::status::clip;

pub(super) fn paint(
    viewport: Viewport,
    keymap: Keymap,
    expanded: bool,
) -> Result<String, std::fmt::Error> {
    let mut lines = vec![if expanded {
        "CORVID COMMANDS | Esc return | press a command key"
    } else {
        "CORVID CMD | ? all commands | Esc return"
    }];
    if keymap.tabs {
        lines.push("w  Find tab       t  New tab       n/p  Next/previous tab");
    }
    if keymap.panes {
        lines.push("%  Split right    \"  Split down    o    Next pane");
    }
    if expanded {
        if keymap.naming {
            lines.push(".  Rename pane    ,  Rename tab (empty name restores auto)");
        }
        if keymap.tabs {
            lines.push("1-9  Select tab by position");
        }
        if keymap.disconnect {
            lines.push("D  Disconnect other clients (keep their terminals running)");
        }
        if keymap.close {
            lines.push("X  End pane    T  End tab    S  End workspace");
            lines.push("   Ending a view stops its terminals and jobs.");
        }
        if keymap.pairing {
            lines.push("r  Pair external controller");
        }
        lines.push("Ctrl-]  Send a literal Ctrl-] to the terminal");
    }
    lines.push("d  Detach (keep terminals running)");
    let rows = usize::from(viewport.rows.get());
    let count = lines.len().min(rows.saturating_sub(1));
    let start = rows.saturating_sub(count);
    let width = usize::from(viewport.cols.get().saturating_sub(1));
    let mut output = String::from("\x1b[?25l\x1b[0m");
    for (offset, line) in lines.into_iter().take(count).enumerate() {
        let style = if offset == 0 { "\x1b[0;7m" } else { "\x1b[0m" };
        write!(
            output,
            "\x1b[{};1H{style}\x1b[2K{}",
            start + offset,
            clip(line, width)
        )?;
    }
    output.push_str("\x1b[0m");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_sheet_stays_inside_small_viewports_and_respects_capabilities() {
        let viewport = Viewport::new(24, 5).unwrap();
        let ansi = paint(viewport, Keymap::default(), true).unwrap();
        let mut parser = vt100::Parser::new(viewport.rows, viewport.cols, 0);
        parser.process(ansi.as_bytes());
        let text = parser.screen().contents();
        assert!(text.contains("CORVID COMMANDS"));
        assert!(text.contains("Detach"));
        assert!(!text.contains("Split"));
        assert!(!text.contains("End pane"));
        assert!(!parser.screen().row_wrapped(0));
    }
}
