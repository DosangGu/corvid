use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, ErrorKind, IsTerminal, Write};
use std::os::windows::io::AsRawHandle;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_core::Viewport;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use windows_sys::Win32::System::Console::{
    ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    FlushConsoleInputBuffer, GetConsoleMode, GetNumberOfConsoleInputEvents, INPUT_RECORD,
    KEY_EVENT, KEY_EVENT_RECORD, ReadConsoleInputW, SetConsoleMode,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_MENU;

pub(crate) struct Tty {
    output_mode: u32,
    input: RefCell<ConsoleInput>,
}

impl Tty {
    pub(crate) fn check() -> Result<Viewport> {
        ensure!(
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            "attach requires a Windows console"
        );
        Self::viewport()
    }

    pub(crate) fn viewport() -> Result<Viewport> {
        let (cols, rows) = size()?;
        Viewport::new(cols, rows).with_context(|| format!("terminal reports {cols}×{rows} cells"))
    }

    pub(crate) fn enter() -> Result<Self> {
        let output = io::stdout().as_raw_handle();
        let mut output_mode = 0;
        // The output handle belongs to this process and the mode pointer is writable.
        checked(unsafe { GetConsoleMode(output, &mut output_mode) })?;
        checked(unsafe {
            SetConsoleMode(
                output,
                output_mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            )
        })?;
        let tty = Self {
            output_mode,
            input: RefCell::new(ConsoleInput::default()),
        };
        enable_raw_mode()?;
        let input = io::stdin().as_raw_handle();
        let mut input_mode = 0;
        // Keep ConPTY's VT input boundaries, including bracketed paste markers.
        // Crossterm restores the original input mode when raw mode is disabled.
        checked(unsafe { GetConsoleMode(input, &mut input_mode) })?;
        checked(unsafe { SetConsoleMode(input, input_mode | ENABLE_VIRTUAL_TERMINAL_INPUT) })?;
        tty.discard_input()?;
        Ok(tty)
    }

    pub(crate) fn discard_input(&self) -> Result<()> {
        // The standard input is a console handle, checked before entering raw mode.
        checked(unsafe { FlushConsoleInputBuffer(io::stdin().as_raw_handle()) })?;
        *self.input.borrow_mut() = ConsoleInput::default();
        Ok(())
    }

    pub(crate) async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.try_read(buffer) {
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(8)).await;
                }
                result => return result,
            }
        }
    }

    pub(crate) fn try_read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let handle = io::stdin().as_raw_handle();
        let mut input = self.input.borrow_mut();
        let mut count = 0;
        for _ in 0..64 {
            while count < buffer.len() {
                let Some(byte) = input.pending.pop_front() else {
                    break;
                };
                buffer[count] = byte;
                count += 1;
            }
            if count == buffer.len() {
                break;
            }
            let mut available = 0;
            // Only this TTY reader consumes the process's console input records.
            checked(unsafe { GetNumberOfConsoleInputEvents(handle, &mut available) })?;
            if available == 0 {
                break;
            }
            let mut record = INPUT_RECORD::default();
            let mut read = 0;
            // A record is available and both output pointers are writable.
            checked(unsafe { ReadConsoleInputW(handle, &mut record, 1, &mut read) })?;
            if read == 0 || record.EventType != KEY_EVENT as u16 {
                continue;
            }
            // EventType identifies the active union member. VT input arrives as
            // UTF-16 key records, including literal ESC and control characters.
            input.key(unsafe { record.Event.KeyEvent });
        }
        if count == 0 {
            return Err(ErrorKind::WouldBlock.into());
        }
        Ok(count)
    }

    pub(crate) async fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut output = io::stdout().lock();
        output.write_all(bytes)?;
        output.flush()
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        let mut output = io::stdout().lock();
        let _ = output.write_all(b"\x1b[?2004l\x1b[?1l\x1b>\x1b[0m\x1b[?25h\x1b[r\x1b[?1049l");
        let _ = output.flush();
        // The output handle remains owned by this process.
        let _ = unsafe { SetConsoleMode(io::stdout().as_raw_handle(), self.output_mode) };
        let _ = disable_raw_mode();
    }
}

#[derive(Default)]
struct ConsoleInput {
    pending: VecDeque<u8>,
    surrogate: Option<u16>,
}

impl ConsoleInput {
    fn key(&mut self, key: KEY_EVENT_RECORD) {
        // ReadConsoleInputW supplies the Unicode member of this union.
        let unit = unsafe { key.uChar.UnicodeChar };
        // ConPTY can deliver non-BMP text as Alt-code key-up events.
        let text_event = key.bKeyDown != 0 || (key.wVirtualKeyCode == VK_MENU && unit != 0);
        if text_event && (unit != 0 || key.wVirtualKeyCode == 0) {
            for _ in 0..key.wRepeatCount {
                self.push(unit);
            }
        }
    }

    fn push(&mut self, unit: u16) {
        if let Some(high) = self.surrogate.take() {
            if (0xdc00..=0xdfff).contains(&unit) {
                let scalar =
                    0x10000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00);
                self.character(char::from_u32(scalar).unwrap());
                return;
            }
            self.character(char::REPLACEMENT_CHARACTER);
        }
        if (0xd800..=0xdbff).contains(&unit) {
            self.surrogate = Some(unit);
        } else {
            self.character(char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER));
        }
    }

    fn character(&mut self, character: char) {
        self.pending
            .extend(character.encode_utf8(&mut [0; 4]).bytes());
    }
}

fn checked(result: i32) -> io::Result<()> {
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_alt_code_key_up_text_without_duplicating_normal_key_releases() {
        let mut input = ConsoleInput::default();
        for unit in "🐦".encode_utf16() {
            let mut key = KEY_EVENT_RECORD {
                wVirtualKeyCode: VK_MENU,
                wRepeatCount: 1,
                ..Default::default()
            };
            key.uChar.UnicodeChar = unit;
            input.key(key);
        }
        let mut release = KEY_EVENT_RECORD {
            wVirtualKeyCode: 65,
            wRepeatCount: 1,
            ..Default::default()
        };
        release.uChar.UnicodeChar = u16::from(b'a');
        input.key(release);
        assert_eq!(
            input.pending.into_iter().collect::<Vec<_>>(),
            "🐦".as_bytes()
        );
    }

    #[test]
    fn console_input_preserves_vt_boundaries_and_split_surrogate_pairs() {
        let mut input = ConsoleInput::default();
        let text = "\x1b[200~한글 🦀\r\x1b[201~\x1d\0";
        for unit in text.encode_utf16() {
            input.push(unit);
        }
        assert_eq!(
            input.pending.into_iter().collect::<Vec<_>>(),
            text.as_bytes()
        );
    }

    #[test]
    fn malformed_surrogate_does_not_consume_the_next_control_character() {
        let mut input = ConsoleInput::default();
        input.push(0xd800);
        input.push(0x1b);
        input.push(0xdc00);
        assert_eq!(
            input.pending.into_iter().collect::<Vec<_>>(),
            "\u{fffd}\x1b\u{fffd}".as_bytes(),
        );
    }
}
