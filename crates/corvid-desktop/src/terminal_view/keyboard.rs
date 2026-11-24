use gpui::{Keystroke, Modifiers};
use qwertty_term_input::key::{Key, KeyEvent};
use qwertty_term_input::key_encode::{self, KittyFlags, Options};
use qwertty_term_input::key_mods::{Mods, OptionAsAlt};

/// Translate a GPUI key press to PTY input. Returning None leaves delivery to
/// the platform text input handler, including dead keys and IME composition.
pub(super) fn encode(stroke: &Keystroke, cursor: bool, keypad: bool) -> Option<Vec<u8>> {
    let modifiers = stroke.modifiers;
    let name = stroke.key.as_str();
    let character = match name {
        "space" => Some(' '),
        _ => single_character(name),
    };
    let key = if let Some(ch) = character {
        if uses_text_input(stroke, ch) {
            return None;
        }
        u8::try_from(u32::from(ch))
            .ok()
            .and_then(Key::from_ascii)
            .unwrap_or(Key::Unidentified)
    } else {
        named_key(name)?
    };
    let event = KeyEvent {
        key,
        mods: Mods {
            shift: modifiers.shift,
            alt: modifiers.alt,
            ctrl: modifiers.control,
            super_: modifiers.platform,
            ..Mods::default()
        },
        unshifted_codepoint: character.map_or(0, u32::from),
        utf8: character.map(|ch| ch.to_string()).unwrap_or_default(),
        ..KeyEvent::default()
    };

    // Keep the conventional encodings used by shells for text shortcuts,
    // backtab, and unmodified keys (including application cursor/keypad mode).
    // Encode other chords with their modifier bits so apps can bind them.
    let legacy = !modifiers.modified()
        || (key == Key::Tab && modifiers == Modifiers::shift())
        || (character.is_some_and(|ch| ch.is_ascii())
            && (modifiers == Modifiers::control() || modifiers == Modifiers::alt()));
    let mut options = Options {
        cursor_key_application: cursor,
        keypad_key_application: keypad,
        alt_esc_prefix: true,
        macos_option_as_alt: OptionAsAlt::True,
        ..Options::default()
    };
    if legacy {
        let bytes = key_encode::encode(&event, &options);
        // Some keys have no distinct legacy representation (e.g. Ctrl+1).
        if !bytes.is_empty() && bytes != event.utf8.as_bytes() {
            return Some(bytes);
        }
    }
    options.kitty_flags = KittyFlags {
        disambiguate: true,
        ..KittyFlags::default()
    };
    let bytes = key_encode::encode(&event, &options);
    (!bytes.is_empty()).then_some(bytes)
}

fn single_character(name: &str) -> Option<char> {
    let mut chars = name.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

fn uses_text_input(stroke: &Keystroke, ch: char) -> bool {
    let modifiers = stroke.modifiers;
    if !modifiers.control && !modifiers.alt && !modifiers.platform {
        return true;
    }
    if !modifiers.alt || modifiers.platform {
        return false;
    }
    // AltGr and Option can produce a layout character or start a dead key.
    // GPUI's text callback will deliver the committed string exactly once.
    stroke.key_char.as_deref().is_some_and(|text| {
        (cfg!(windows) && modifiers.control && !text.is_empty())
            || single_character(text)
                .is_none_or(|typed| !typed.is_control() && !typed.eq_ignore_ascii_case(&ch))
    })
}

fn named_key(name: &str) -> Option<Key> {
    // GPUI spellings that differ from the encoder's key names.
    let name = match name {
        "up" => "arrow_up",
        "down" => "arrow_down",
        "left" => "arrow_left",
        "right" => "arrow_right",
        "pageup" => "page_up",
        "pagedown" => "page_down",
        "menu" => "context_menu",
        "add" => "numpad_add",
        "subtract" => "numpad_subtract",
        "multiply" => "numpad_multiply",
        "divide" => "numpad_divide",
        "decimal" => "numpad_decimal",
        other => other,
    };
    Key::from_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_terminal_keys_and_unclaimed_chords() {
        for (stroke, expected) in [
            ("enter", b"\r".as_slice()),
            ("shift-enter", b"\x1b[13;2u"),
            ("ctrl-enter", b"\x1b[13;5u"),
            ("alt-backspace", b"\x1b[127;3u"),
            ("shift-tab", b"\x1b[Z"),
            ("ctrl-home", b"\x1b[1;5H"),
            ("shift-f5", b"\x1b[15;2~"),
            ("ctrl-v", b"\x16"),
            ("ctrl-j", b"\n"),
            ("ctrl-]", b"\x1d"),
            ("alt-b", b"\x1bb"),
            ("ctrl-shift-i", b"\x1b[105;6u"),
            ("super-x", b"\x1b[120;9u"),
        ] {
            let key = Keystroke::parse(stroke).unwrap();
            assert_eq!(
                encode(&key, false, false).as_deref(),
                Some(expected),
                "{stroke}"
            );
        }
        let up = Keystroke::parse("up").unwrap();
        assert_eq!(encode(&up, false, false).unwrap(), b"\x1b[A");
        assert_eq!(encode(&up, true, false).unwrap(), b"\x1bOA");
    }

    #[test]
    fn leaves_layout_text_and_composition_to_the_platform() {
        for stroke in [
            "a",
            "shift-a",
            "space",
            "한",
            "ㅅ",
            "t->ㅅ",
            "dead_acute",
            "ctrl-alt-q->@",
            "alt-s->ß",
        ] {
            assert!(
                encode(&Keystroke::parse(stroke).unwrap(), false, false).is_none(),
                "{stroke}"
            );
        }
    }
}
