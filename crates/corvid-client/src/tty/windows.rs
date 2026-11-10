use std::io::{self, ErrorKind, IsTerminal, Write};
use std::os::windows::io::AsRawHandle;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_core::Viewport;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use windows_sys::Win32::System::Console::{
    ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, FlushConsoleInputBuffer,
    GetConsoleMode, SetConsoleMode,
};

pub(crate) struct Tty {
    output_mode: u32,
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
        let tty = Self { output_mode };
        enable_raw_mode()?;
        tty.discard_input()?;
        Ok(tty)
    }

    pub(crate) fn discard_input(&self) -> Result<()> {
        // The standard input is a console handle, checked before entering raw mode.
        checked(unsafe { FlushConsoleInputBuffer(io::stdin().as_raw_handle()) })?;
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
        let mut count = 0;
        for _ in 0..64 {
            if buffer.len() - count < 16 || !event::poll(Duration::ZERO)? {
                break;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let bytes = encode_key(key);
            if bytes.is_empty() {
                continue;
            }
            buffer[count..count + bytes.len()].copy_from_slice(&bytes);
            count += bytes.len();
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

fn encode_key(key: KeyEvent) -> Vec<u8> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(control);
    let sequence = match key.code {
        KeyCode::Up => Some(('A', 0)),
        KeyCode::Down => Some(('B', 0)),
        KeyCode::Right => Some(('C', 0)),
        KeyCode::Left => Some(('D', 0)),
        KeyCode::Home => Some(('H', 0)),
        KeyCode::End => Some(('F', 0)),
        KeyCode::Insert => Some(('~', 2)),
        KeyCode::Delete => Some(('~', 3)),
        KeyCode::PageUp => Some(('~', 5)),
        KeyCode::PageDown => Some(('~', 6)),
        KeyCode::F(number @ 5..=12) => Some((
            '~',
            [15, 17, 18, 19, 20, 21, 23, 24][usize::from(number - 5)],
        )),
        _ => None,
    };
    if let Some((final_byte, number)) = sequence {
        return match (modifier, number) {
            (1, 0) => format!("\x1b[{final_byte}"),
            (1, _) => format!("\x1b[{number}{final_byte}"),
            (_, 0) => format!("\x1b[1;{modifier}{final_byte}"),
            _ => format!("\x1b[{number};{modifier}{final_byte}"),
        }
        .into_bytes();
    }
    let mut bytes = match key.code {
        KeyCode::Char(character) if control && !alt && character.is_ascii() => {
            match character.to_ascii_uppercase() {
                '@'..='_' => vec![character.to_ascii_uppercase() as u8 & 0x1f],
                ' ' => vec![0],
                '?' => vec![0x7f],
                _ => character.to_string().into_bytes(),
            }
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::F(number @ 1..=4) => {
            let final_byte = char::from(b'P' + number - 1);
            return if modifier == 1 {
                format!("\x1bO{final_byte}").into_bytes()
            } else {
                format!("\x1b[1;{modifier}{final_byte}").into_bytes()
            };
        }
        _ => return Vec::new(),
    };
    if alt && !control {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn checked(result: i32) -> io::Result<()> {
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
