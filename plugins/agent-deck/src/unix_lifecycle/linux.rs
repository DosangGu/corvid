use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, ensure};
use corvid_async::time::timeout;
use corvid_protocol::COMMAND_TIMEOUT;
use semver::Version;
use tokio::process::Command;

pub(super) struct Peer {
    pid: u32,
    handle: io::Result<OwnedFd>,
}

impl Peer {
    pub(super) fn new(pid: u32) -> Self {
        // Keep a stable kernel reference for older Decks without retirement IPC.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        let handle = if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
        };
        Self { pid, handle }
    }

    pub(super) fn visible(&self) -> bool {
        // The Linux backend opens a window even in background mode because
        // it has no tray surface.
        true
    }

    pub(super) async fn retire_legacy(&self, current: &Version) -> Result<Option<bool>> {
        let executable = PathBuf::from(format!("/proc/{}/exe", self.pid));
        let image = std::fs::read_link(&executable)?;
        ensure!(
            image.file_name().is_some_and(|name| {
                name == "corvid-agent-deck" || name == "corvid-agent-deck (deleted)"
            }),
            "Agent Deck endpoint belongs to an unexpected executable"
        );
        let output = timeout(
            COMMAND_TIMEOUT,
            Command::new(executable)
                .arg("--version")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("checking the running Agent Deck version timed out")??;
        ensure!(
            output.status.success(),
            "cannot check the running Agent Deck version"
        );
        let found = std::str::from_utf8(&output.stdout)?
            .trim()
            .strip_prefix("corvid-agent-deck ")
            .context("unexpected Agent Deck version response")?;
        if Version::parse(found)?.cmp_precedence(current).is_ge() {
            return Ok(None);
        }
        let handle = self.handle.as_ref().map_err(|error| {
            anyhow::anyhow!("could not identify the running Agent Deck process: {error}")
        })?;
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                handle.as_raw_fd(),
                libc::SIGTERM,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        ensure!(
            sent == 0,
            "retiring Agent Deck: {}",
            io::Error::last_os_error()
        );
        Ok(Some(self.visible()))
    }
}
