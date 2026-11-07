mod pty;
mod screen;
mod tty;
mod viewport;

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, poll};
use portable_pty::CommandBuilder;
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};

use crate::pty::TerminalPty;
use crate::screen::TerminalScreen;
use crate::tty::OuterTty;
use crate::viewport::Viewport;

const BUFFER_BYTES: usize = 8192;
const INPUT_LIMIT: usize = 128 * 1024;
const FRAME_INTERVAL: Duration = Duration::from_millis(34);

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("corvid-terminal-spike: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u32> {
    let shutdown = Arc::new(AtomicBool::new(false));
    for signal in [SIGHUP, SIGINT, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))?;
    }

    let viewport = OuterTty::viewport().context("reading initial outer terminal size")?;
    let tty = OuterTty::enter()?;
    let pty = TerminalPty::spawn(child_command(), viewport).context("starting child PTY")?;
    let screen = TerminalScreen::new(viewport);

    interact(tty, pty, screen, viewport, &shutdown)
}

fn child_command() -> CommandBuilder {
    let mut args = std::env::args_os().skip(1);
    let mut command = match args.next() {
        Some(program) => {
            let mut command = CommandBuilder::new(program);
            command.args(args);
            command
        }
        None => CommandBuilder::new_default_prog(),
    };

    command.env("TERM", "corvid");
    command.env("COLORTERM", "truecolor");
    command
}

fn interact(
    mut tty: OuterTty,
    mut pty: TerminalPty,
    mut screen: TerminalScreen,
    mut viewport: Viewport,
    shutdown: &AtomicBool,
) -> Result<u32> {
    let stdin = io::stdin();
    let mut outgoing = VecDeque::new();
    let mut buffer = [0; BUFFER_BYTES];
    let mut dirty = true;
    let mut next_frame = Instant::now();
    let mut eof = false;

    while !shutdown.load(Ordering::Relaxed) {
        if let Ok(size) = OuterTty::viewport()
            && size != viewport
        {
            pty.resize(size)?;
            screen.resize(size);
            viewport = size;
            dirty = true;
        }

        let interest = pty_interest(eof, outgoing.is_empty());
        let (stdin_ready, pty_ready) = wait_for_io(&stdin, &pty, interest)?;
        if pty_ready.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR) {
            let count = read_child(&mut pty, &mut buffer)?;
            eof = count == Some(0);
            if let Some(count) = count.filter(|count| *count > 0) {
                let replies = screen.process(&buffer[..count])?;
                enqueue(&mut outgoing, &replies)?;
                dirty = true;
            }
        }

        if !eof && stdin_ready {
            match read_keyboard(&stdin, &mut buffer)? {
                Some(0) => return Ok(0),
                Some(count) => enqueue(&mut outgoing, &buffer[..count])?,
                None => {}
            }
        }
        if !eof {
            flush_input(&mut pty, &mut outgoing)?;
        }

        if dirty && Instant::now() >= next_frame {
            tty.write(&screen.redraw()?)?;
            next_frame = Instant::now() + FRAME_INTERVAL;
            dirty = false;
        }

        if eof && let Some(status) = pty.try_wait()? {
            if dirty {
                tty.write(&screen.redraw()?)?;
            }
            return Ok(status.exit_code());
        }
    }

    Ok(0)
}

fn read_child(pty: &mut TerminalPty, buffer: &mut [u8]) -> Result<Option<usize>> {
    match pty.read(buffer) {
        Ok(count) => Ok(Some(count)),
        Err(error) if is_transient(&error) => Ok(None),
        Err(error) => Err(error).context("reading child PTY"),
    }
}

fn read_keyboard(stdin: &io::Stdin, buffer: &mut [u8]) -> Result<Option<usize>> {
    match nix::unistd::read(stdin, buffer) {
        Ok(count) => Ok(Some(count)),
        Err(Errno::EINTR | Errno::EAGAIN) => Ok(None),
        Err(error) => Err(error).context("reading outer TTY"),
    }
}

fn pty_interest(eof: bool, writes_empty: bool) -> Option<PollFlags> {
    if eof {
        return None;
    }

    let mut flags = PollFlags::POLLIN;
    if !writes_empty {
        flags |= PollFlags::POLLOUT;
    }
    Some(flags)
}

fn wait_for_io(
    stdin: &io::Stdin,
    pty: &TerminalPty,
    interest: Option<PollFlags>,
) -> Result<(bool, PollFlags)> {
    let mut fds = [
        PollFd::new(stdin.as_fd(), PollFlags::POLLIN | PollFlags::POLLHUP),
        PollFd::new(pty.as_fd(), interest.unwrap_or_else(PollFlags::empty)),
    ];
    let active = if interest.is_some() { 2 } else { 1 };

    match poll(&mut fds[..active], 20u16) {
        Ok(_) => {
            let stdin_flags = fds[0].revents().unwrap_or_else(PollFlags::empty);
            let pty_flags = fds[1].revents().unwrap_or_else(PollFlags::empty);
            ensure!(
                !stdin_flags.contains(PollFlags::POLLNVAL),
                "outer TTY closed"
            );
            ensure!(!pty_flags.contains(PollFlags::POLLNVAL), "child PTY closed");
            Ok((!stdin_flags.is_empty(), pty_flags))
        }
        Err(Errno::EINTR) => Ok((false, PollFlags::empty())),
        Err(error) => Err(error).context("polling terminals"),
    }
}

fn enqueue(outgoing: &mut VecDeque<u8>, bytes: &[u8]) -> Result<()> {
    ensure!(
        outgoing.len() + bytes.len() <= INPUT_LIMIT,
        "PTY input queue limit exceeded"
    );
    outgoing.extend(bytes);
    Ok(())
}

fn flush_input(pty: &mut TerminalPty, outgoing: &mut VecDeque<u8>) -> Result<()> {
    if outgoing.is_empty() {
        return Ok(());
    }

    let (first, _) = outgoing.as_slices();
    match pty.write(first) {
        Ok(0) => anyhow::bail!("PTY write made no progress"),
        Ok(count) => {
            outgoing.drain(..count);
            Ok(())
        }
        Err(error) if is_transient(&error) => Ok(()),
        Err(error) => Err(error).context("writing child PTY"),
    }
}

fn is_transient(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock)
}
