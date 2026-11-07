# Local Companion

The companion connects to one configured Linux Corvid daemon and maintains
virtual slots. It serves framed Protobuf to the local CLI over a private Unix
socket on Linux or a current-user named pipe on Windows.

## Configure and Run

Read the daemon's `host_id` with `corvid control host` on the execution host.

### Linux

For a local daemon, initialize the companion with that ID. The global
`--runtime-dir` flag selects a local daemon's socket directory when needed.

```sh
corvid companion init --host-id "$HOST_ID" --count 8
corvid companion run
```

For a remote daemon, add `--ssh-alias work-host` to `init`. Configure the alias
in OpenSSH and establish host-key trust and noninteractive authentication first.
Install `corvid` on the remote command PATH and start its daemon. The companion
opens a separate `ssh -T` link with the fixed remote command `corvid control --stdio`.
The relay finds the daemon's sockets through the default XDG runtime path or
the remote command environment's `CORVID_RUNTIME_DIR`.

Defaults are `$XDG_RUNTIME_DIR/corvid-companion` for the presentation socket and
`$XDG_STATE_HOME/corvid/companion` for state, with `$HOME/.local/state` as the
state base fallback. Override with `companion --socket-dir` and `--state-dir`.
Point subsequent commands at the same socket directory.

### Windows

Build with `cargo build --locked -p corvid`. Enable the Windows OpenSSH Client
feature and configure `work-host` for noninteractive authentication. The
companion uses the OpenSSH executable in the Windows system directory.
[Windows OpenSSH setup](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse?pivots=windows-11)

```powershell
corvid.exe companion init --host-id $HostId --ssh-alias work-host --count 8
corvid.exe companion run
# In a second PowerShell window
corvid.exe companion slots
```

State defaults to `%LOCALAPPDATA%\Corvid\companion`. For `--state-dir`, give
`init` a new absolute directory on a local fixed drive with persistent ACLs.
Use the same override for `run`. The companion requires a protected DACL
granting access only to the current user on its state directory and files.
Set `--pipe-instance` on `run` and client commands if using a pipe instance
other than `main`. Separate companion instances also need separate state
directories. Both ends check the pipe owner and DACL before exchanging
frames. [Named pipe access controls](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)

## Pair and Focus

In the intended attached TUI, press Ctrl-] then r. Run `corvid companion pair`
and enter its private code on stdin. The code expires in 60 seconds. Dismiss
the modal with Enter or Esc and keep that TUI attached.

```sh
corvid companion slots --count 8
corvid companion focus-slot --slot "$SLOT_CAPTURE" --binding "$BINDING_CAPTURE"
```

Copy `capture=` from the desired `slot=` line and the explicitly chosen
`binding` line. The TUI must already hold the target workspace's writer lease.
`remote_selection=Applied local_activation=ManualRequired` means the existing
TUI selected the run. Bring that terminal forward manually. Refresh the
captures after reconnect, restart, or reassignment. A stale click is rejected.

To acknowledge one alert, use the displayed `control_capture`, `run`, and
`latest_attention_id` with `companion ack --control`, `--run`, and
`--attention-id`. An input request stays pending until the provider reports a
matching result. Use `companion unpair --binding "$BINDING_CAPTURE"` to revoke
one registration.

## Slot Management and Recovery

- `slots --overflow-page 0` shows the first overflow page. Use `pin` and `unpin`
  with `--slot "$SLOT_CAPTURE"` to reserve or release an empty placement.
- `remove --slot "$SLOT_CAPTURE"` suppresses that run. Use `remove-overflow --run`
  for an overflow entry and `readd --run` to make a current suppressed run
  eligible again. `place --slot "$EMPTY_CAPTURE" --run "$RUN_KEY"` assigns a
  current overflow run to an explicitly selected empty slot.
- The companion keeps stable IDs and slot revisions in private JSON files.
  Windows flushes a same-directory temporary file before replacing the
  destination with `MoveFileExW`. On restart, the companion reads placements
  and synchronizes before enabling focus. Badges are rebuilt from the remote
  snapshot. A new daemon epoch makes old slots historical until there is end
  evidence or explicit removal.
  [Windows file replacement flags](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw)
- A broken control channel leaves slots visible with stale state. Reconnection
  takes a full snapshot, revalidates surviving registrations, and issues fresh
  captures. A TUI detach retires its registration. Pair the replacement TUI
  after reattaching.

## Verification

The workspace integration test verifies separate placements for two managed
agents with the same working directory, followed by exit and slot reuse.
The Claude Code 2.1.318 fixture verifies pairing, companion restart, exact
question focus and acknowledgment, and rejection of a stale click after
`/clear`. The OpenSSH 10.6p1 loopback fixture kills the companion's SSH process
and verifies full resync under the same daemon epoch while the separate TUI
and its PTY remain active. These checks ran on Linux.

The CLI and companion tests cross-link for `x86_64-pc-windows-gnu` with
Rust 1.93. Wine 11.21 ran CLI help, private pipe I/O, and rejection of default
pipe and state-directory ACLs. Its pipe implementation accepted a duplicate
first instance, and reopening a freshly created state directory returned a
DACL with an extra ACE. The storage tests fail closed in that environment.

Run `cargo test --locked -p corvid-companion` on native Windows to verify
pipe exclusivity, state replacement and recovery, and two presentation clients.
Native Windows storage, OpenSSH runtime, terminal activation, desktop
presentation, and hardware integration remain unqualified.
