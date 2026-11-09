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
corvid.exe companion hosts
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

## Inspect the Configured Host

Run `corvid companion hosts` against the running companion. The `ListHosts`
presentation request returns one configured HostId, the endpoint type (`local`
or `ssh` with its alias), and the control connection state with any issue code.
When the watch is ready, the row also includes the verified remote epoch and
snapshot sequence. The query reads the companion's current state without opening
an additional control connection. The presentation capability is `list_hosts.v1`.

## Desktop Slot Board

Build the separate native desktop binary with `cargo build --locked -p corvid-desktop`.
Start the configured companion, pair the intended TUI, and run `corvid-desktop`
from the build output. The desktop accepts the same `--socket-dir` on Linux or
`--pipe-instance` on Windows as the companion. The board uses eframe 0.33.3 with
the Glow renderer and connects directly to the private Protobuf endpoint.
[eframe source](https://github.com/emilk/egui/tree/0.33.3/crates/eframe)

Choose a paired terminal in **Target terminal**, then click **Focus** on the
intended slot. The result bar reports remote selection and local activation.
**Acknowledge alert** marks that alert seen. Answer questions and grant
permissions in the terminal. Slot cards support pin and remove. Use **Overflow**
to page through unassigned or removed runs, re-add a removed run, or choose an
overflow run and place it into an empty slot. **Recent ended** shows recent run
IDs released from the layout.

On disconnection the board keeps the last snapshot visible and disables its
actions. Reconnection loads a fresh snapshot and requires an explicit terminal
choice. A gesture uses the slot and terminal revisions that were visible when
it began. A stale action is rejected and can be retried as a new click after
reviewing the refreshed board.

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

### WezTerm on Windows

Keep the ordinary SSH session and its Corvid TUI open. In a separate local
shell in the same WezTerm GUI, list panes and choose the numeric ID for the
pane containing the TUI. Read its pairing code as above. Run the following
locally, with the chosen ID in `$PaneId`.

```powershell
wezterm --skip-config cli --no-auto-start list --format json
corvid.exe companion pair --wezterm-pane $PaneId
```

Use `--wezterm-socket` with that GUI's `WEZTERM_UNIX_SOCKET` value when the
pairing shell is outside the target WezTerm instance. The companion checks
the running GUI's user, process, and socket peer and records the exact
window, tab, and pane IDs. It uses its pairing record before and after remote
focus. If the pane moves, closes, or belongs to a replacement process,
pair the intended pane again. [WezTerm CLI instance selection](https://wezterm.org/cli/cli/index.html)

The same `focus-slot` command selects the registered WezTerm pane after
remote focus succeeds. `Activated` means Windows reported the registered
window in the foreground. `PaneSelectedForegroundUnknown` means the pane
selection succeeded and Windows foreground activation could not be confirmed.
The slot output shows the registered PID and window/tab/pane IDs to locate
it. Windows activation is attempted only for a single unambiguous GUI window
with WezTerm's default window class.
`Unavailable` after `Applied` means the remote selection succeeded but the
local activation could not be completed. [WezTerm pane activation](https://wezterm.org/cli/cli/activate-pane.html)
[Windows foreground restrictions](https://learn.microsoft.com/en-us/windows/win32/winmsg/window-features)

Local WezTerm and manual registrations are kept in a protected, volatile registry key
for the current Windows logon. After a companion restart, refresh the slot
and binding captures once the daemon synchronizes. A matching remote binding
can recover automatic activation in the same logon. Before remote focus the
companion checks the process creation time, socket file identity and peer PID,
and the exact window, tab, and pane. If the local record is missing or
unreadable, focus returns `LocalTerminalUnavailable` before changing the
remote view. Pair again after a failed identity check, logoff, or reboot.
Manual bindings created by older versions require a fresh pairing once.
If the registry is unavailable, a new pairing works for the current
companion process.
[Volatile registry keys](https://learn.microsoft.com/en-us/windows/win32/api/winreg/nf-winreg-regcreatekeyexw)
[Windows logon identity](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-token_statistics)

## Slot Management and Recovery

- `slots --overflow-page 0` shows the first overflow page. Use `pin` and `unpin`
  with `--slot "$SLOT_CAPTURE"` to reserve or release an empty placement.
- `remove --slot "$SLOT_CAPTURE"` suppresses that run. Use `remove-overflow --run`
  for an overflow entry and `readd --run` to make a current suppressed run
  eligible again. `place --slot "$EMPTY_CAPTURE" --run "$RUN_KEY"` assigns a
  current overflow run to an explicitly selected empty slot.
- The companion keeps stable IDs and slot revisions in private JSON files.
  Windows flushes a same-directory temporary file before replacing the
  destination with `MoveFileExW`. The optional cache in `slots.json` stores
  up to 512 last-known observations, with assigned slots first and overflow
  in eligibility order. It stores badges, evidence, counts, and timestamps.
  The companion groups observation saves on a one-second timer and flushes
  pending observations on clean exit. Existing placement files load with an
  empty cache. A new daemon epoch makes old slots historical until there is
  end evidence or explicit removal.
  [Windows file replacement flags](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw)
- During a control outage, desktop cards show the last known badge and
  observation age. `companion slots` prints `last_observed_unix_ms`. A fresh
  snapshot revalidates surviving registrations and enables focus and alert
  acknowledgment with new captures. A TUI detach retires its registration.
  Pair the replacement TUI after reattaching.

## Verification

The workspace integration test verifies separate placements for two managed
agents with the same working directory, followed by exit and slot reuse.
The Claude Code 2.1.318 fixture verifies pairing, companion restart, exact
question focus and acknowledgment, and rejection of a stale click after
`/clear`. The OpenSSH 10.6p1 loopback fixture kills the companion's SSH process
and verifies full resync under the same daemon epoch while the separate TUI
and its PTY remain active. These checks ran on Linux.

The A10 wire fixture uses two attached TUIs and a real companion. A focus
request with no registrations returns `NoAttachedClient`. Two registrations
without an explicit choice return `AmbiguousClient`. A wrong revision or
retired capture returns `StaleBinding`. After one TUI detaches, a request
still needs an explicit choice. These failures leave the daemon's snapshot
and sequence unchanged.

The A25 offline restart fixture holds a question open in Claude Code 2.1.318
while the control socket is unavailable. A new companion restores the slot,
badge, counts, and observation time. Attempts to focus or acknowledge return
`ConnectionRequired`. The original TUI selection and pending question survive.
After the control socket returns, a fresh capture focuses the same question.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid/tests/claude_qualification.py \
  --corvid target/debug/corvid --scenario offline-restart
```

Linux desktop verification used Xvfb 21.1.25 with a synthetic Protobuf companion.
The GUI retained the captured run and revisions above 2^53 across a slot
replacement during a mouse press. A fresh click used the replacement's
revision. Dropping the IPC connection during a press produced no replay
after reconnect. A desktop pin also persisted through the real Linux companion.
The A29 version-skew fixture sent an unknown focus activation result and
then an unknown run availability. The desktop kept the last valid snapshot
as a cache and disabled actions. A compatible snapshot restored the link,
and the user had to select the terminal again before focusing.
`cargo test --locked -p corvid-desktop` checks capture, connection fencing,
and input-frame behavior without a display. The Windows desktop binary
cross-linked for `x86_64-pc-windows-gnu`. Wine 11.21 ran its four tests and
rendered the offline window.

The CLI and companion tests cross-link for `x86_64-pc-windows-gnu` with
Rust 1.93. Wine 11.21 ran CLI help, private pipe I/O, and rejection of default
pipe and state-directory ACLs. Its pipe implementation accepted a duplicate
first instance, and reopening a freshly created state directory returned a
DACL with an extra ACE. The storage tests fail closed in that environment.

Windows PE build `10.0.26100.9457` from the Windows 11 Enterprise 26H2
evaluation ISO ran these cross-built tests in QEMU 11.1.3. With the
temporary directory on a dedicated NTFS volume, both named-pipe integration
tests and all four slot-storage tests passed. The tests covered exclusive
pipe ownership, two presentation clients, private directory ACLs, and state
replacement and recovery. The library passed nine tests, including exact
file identity and cross-process registry restore, with `SystemTemp` on the
NTFS volume for the SYSTEM account. The Windows PE image lacked
`afunix.sys` and the peer PID test returned socket error 10047. The desktop
test executable exited before running its tests with `0xc0000135`.

The three registration tests also passed in Windows PE after the schema
update. A new process restored both explicit manual and WezTerm records.
The tests checked legacy WezTerm records, exact revisions, and rejection of
missing pane fields and unsupported versions.

Wine read a protected registry key prepared by the test and cleared it on
session restart. Wine discarded the security descriptor when creating a new
key, so the companion rejected the key.

On 2026-11-09, Windows x64 build `26300.9711` (26H2) ran 66 tests across the
CLI, client, companion, core, desktop, and protocol packages using MSVC Rust
1.93.1 and `protoc` 36.1. A non-elevated account used an NTFS temporary
directory. Named-pipe ownership and client access, private state ACLs, slot
replacement and recovery, and same-logon registry restore across processes
passed. The AF_UNIX test also passed after accounting for Winsock reporting
zero output bytes while writing the peer PID.

The WezTerm adapter follows the per-process socket layout and CLI options in
WezTerm `20240203-110809-5046fc22`.
[WezTerm GUI socket source](https://github.com/wezterm/wezterm/blob/20240203-110809-5046fc22/wezterm-gui/src/main.rs)
[WezTerm CLI option source](https://github.com/wezterm/wezterm/blob/20240203-110809-5046fc22/wezterm/src/cli/mod.rs)

On an interactive Windows desktop, point the native WezTerm fixture at the
installed GUI executable and record the Windows and WezTerm versions. Use its
absolute path if `wezterm-gui.exe` is outside `PATH`.

```powershell
$env:CORVID_WEZTERM_GUI = (Get-Command wezterm-gui.exe -ErrorAction Stop).Source
& (Join-Path (Split-Path $env:CORVID_WEZTERM_GUI) 'wezterm.exe') --version
Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,BuildNumber
cargo test --locked -p corvid-companion --lib native_wezterm_activation_keeps_exact_gui_and_pane -- --ignored --nocapture
```

The fixture starts separate WezTerm instances with its own test panes and
checks that a colliding pane ID in another GUI keeps its selection. It checks
rejection after a pane move, closure, or GUI replacement and the foreground
fallback when the registered GUI owns multiple windows. On Windows build
`26300.9711` the fixture passed with WezTerm `20240203-110809-5046fc22`. It
reported `Activated` for the single-window case and
`PaneSelectedForegroundUnknown` for the multiple-window case.
[WezTerm new-process option](https://wezterm.org/cli/start.html)

Windows system OpenSSH `9.5p2` reached the expected host and epoch on an
Ubuntu 26.04.1 WSL daemon over loopback SSH. A WezTerm SSH TUI with a
synthetic agent paired successfully. `focus-slot` selected the original
Corvid tab and reported `Activated`. Restarting the companion restored the
pin and exact WezTerm registration, and fresh captures still focused the
original terminal. The old captures were rejected. After the WezTerm pane
moved, focus returned `LocalTerminalUnavailable` before changing the remote
selection. The desktop board opened against the same named pipe. Desktop
button interactions and the Windows reboot case remain open.

### Local PC reboot reproduction

Use a dedicated Linux daemon on a separate machine and a Windows companion
with its own state directory and pipe instance. Leave a verified Claude Code
question pending in the only attached TUI. Create a second Corvid tab with
Ctrl-] then t and return to the question with Ctrl-] then p. Pair that TUI
to its WezTerm pane and pin the question's slot using `companion pin --slot`.
From another shell in that WezTerm GUI, capture the starting state. Use
the actual slot number and put the checkpoint outside the repository.

```powershell
$checkpoint = Join-Path $env:LOCALAPPDATA 'corvid-a25-reboot.json'
wezterm --version
& "$env:SystemRoot\System32\OpenSSH\ssh.exe" -V
py -3 crates/corvid/tests/local_reboot.py before $checkpoint --ssh-alias work-host --pipe-instance a25 --slot 1
```

Restart Windows manually. From PowerShell, start the same companion with
the same pipe instance and state directory. Keep WezTerm closed and run
`recovered` with the same checkpoint, alias, and pipe instance. The fixture
checks the new local boot time, the saved pin and slot, the unchanged remote
daemon and pending request, and rejection of the retired registration.

Open WezTerm and run `ssh -tt work-host corvid attach`. Pair the new TUI to
its pane. If the question is visible, switch to the existing second tab
with Ctrl-] then n. From a shell in that GUI, run `repaired` with the same
arguments. The fixture checks that the old identity cannot change the TUI
and the new registration selects the original run, terminal, and WezTerm
pane. It checks the pane inventory and pending request again after focus.
Record the reported activation outcome and the Windows build and boot times
from the checkpoint. Keep the checkpoint local.

On a Linux controller, use `python3` and `--socket-dir` in place of the
Windows launcher and pipe option. Pair the TUI without a WezTerm target.
A25 recovery case C passed with a Debian 13.7 controller in QEMU 11.1.3
using OpenSSH 10.0p2. Only the controller guest rebooted. The separate
Linux execution host used OpenSSH 10.6p1 and Claude Code 2.1.318. The saved
pin, slot revision, agent PID, and pending RequestId survived. A newly
paired SSH TUI selected the original question. The request resolved after
the answer was entered in that TUI.

## Deferred Platform Verification

### Installed Windows

Use the desktop board with a paired SSH TUI to verify focus, held-gesture
fencing, overflow, and restart recovery. Capture the activation outcome and
confirm that focus preserves the open terminal inventory and a pending remote
provider question. Follow the [local reboot procedure](#local-pc-reboot-reproduction)
with a separate Linux execution host to confirm that a new Windows logon
restores slots and requires a new pairing.

### macOS

When the separate macOS execution work is scheduled, record the OS build,
CPU architecture, terminal emulator, and SSH version. Exercise the pinned
terminal compatibility suite (A19) on that combination. Qualify private
local IPC and peer identity (A24), SSH loss and last logout with the chosen
service manager (A21), and input fencing across resize and reattach (A20).
Run the local companion, desktop, and recovery suites on macOS after their
platform paths are implemented.
