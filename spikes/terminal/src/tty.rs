use std::io::{self, IsTerminal, Stdout, Write};

use anyhow::{Result, ensure};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};

use corvid_core::Viewport;

pub struct OuterTty {
    stdout: Stdout,
}

impl OuterTty {
    pub fn enter() -> Result<Self> {
        let stdout = io::stdout();
        ensure!(
            io::stdin().is_terminal() && stdout.is_terminal(),
            "a terminal is required"
        );
        enable_raw_mode()?;

        let mut tty = Self { stdout };
        tty.write("\x1b[?1049h")?;
        Ok(tty)
    }

    pub fn viewport() -> Result<Viewport> {
        let (cols, rows) = size()?;
        Ok(Viewport::new(cols, rows)?)
    }

    pub fn write(&mut self, ansi: &str) -> io::Result<()> {
        self.stdout.write_all(ansi.as_bytes())?;
        self.stdout.flush()
    }
}

impl Drop for OuterTty {
    fn drop(&mut self) {
        let _ = self.write("\x1b[?2004l\x1b[?1l\x1b>\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = disable_raw_mode();
    }
}
