# Continuous integration coverage

**Corvid Checks** always reports lightweight PR validation. **Corvid Linux** and
**Corvid Windows** run native x64 and ARM64 jobs, and **Corvid macOS** runs native
Apple Silicon jobs. Each workflow defines its own Runtime and Desktop jobs
and skips documentation-only PRs.

**Package Corvid** runs installer integration separately for PRs touching
`Cargo.lock` or package inputs and for relevant pushes to `main`. The tag-only
**Publish Corvid Release** workflow calls it to build the assets it publishes.
See the [workflow trigger table](05_release_distribution.md#build-and-publish).

## Checks on source pull requests

| Platform | Automated behavior |
| --- | --- |
| All five targets | Runtime and workspace Rust tests, Clippy, and native builds. Tests cover protocol framing, identities, input fencing, and restoration through the daemon/client boundary. |
| Windows | `windows_daemon.rs` runs real ConPTY sessions and private named pipes, including input/paste isolation, tab naming, detach/resume, multiple writers, provider observations, and daemon handoff. |
| Linux and macOS | The terminal spike and `interactive.py --tty-only` exercise real PTYs: fragmented UTF-8, escape sequences, bracketed paste, terminal queries, resize, Ctrl-C, exit status, TTY restoration, pairing, tabs, and reattachment to the same child. Linux runs the full interactive suite instead of the TTY-only subset. |
| Linux | The full interactive suite also checks automatic daemon startup, shell continuity across replacement, and synthetic agent notifications. The loopback OpenSSH fixture checks abrupt disconnects, companion resynchronization, and same-PTY reattachment under a transient systemd user service. |
| Linux Desktop | Xvfb/Openbox tests drive the real application with keyboard and clipboard events. They check Unicode paste, tab creation and selection, independent windows, the new-window menu, settings, close/quit/relaunch, saved window count, and retained shell variables. |

The Python integration drivers are explicit workflow steps; `cargo test` alone
does not run them. They have ten-minute step limits. The UI test checks observable
state and exact synthetic output instead of comparing font-dependent pixels.
It uses a private home/state directory, temporary daemon, and separate X11 display.
The SSH fixture generates temporary keys and pins its loopback server's host key;
it does not need repository secrets or an external host.

Title regressions exercise a paused consumer, a 256-update burst, a blocked
Desktop render thread, injected delivery delay, subscription recovery, and daemon
epoch rejection. Native PTY tests cover title publication and reattachment without
new output. Linux Desktop additionally runs the title path over real loopback SSH
with hosted PTYs. These tests exercise Desktop's state delivery, not screen paint
latency or a comparison against another terminal multiplexer.

TTY restoration compares every mode, speed, and control character. On macOS,
only the kernel-managed `PENDIN` state bit is excluded: Darwin sets it when
canonical input is restored, independently of the saved configuration.
See [Darwin's terminal attribute handling](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/tty.c).

These checks provide portions of A19 (terminal compatibility), A20 (input
isolation), A21 (disconnect survival), A25 (Desktop recovery), and A32 (shared
workspace actions). They do not establish every acceptance case in those rows.

## Additional packaging checks

Package integration tests installation, corruption rejection, version switching,
and daemon lifecycle on all three platforms. Linux repeats the Desktop UI suite
against the archive. Windows validates executable icons and Setup
install/upgrade/uninstall. macOS validates the DMG, signatures, architectures,
bundled runtimes, and a real Desktop window.

## Reproduce locally

From a checkout in a trusted directory, build the runtime and terminal spike:

```sh
cargo build --locked -p corvid-tui -p corvid-terminal-spike
python3 spikes/terminal/tests/interactive.py target/debug/corvid-terminal-spike
python3 crates/corvid-tui/tests/interactive.py --tty-only target/debug/corvid
```

On Linux, run `interactive.py` without `--tty-only` for the additional process
and provider cases. `ssh_lifecycle.py target/debug/corvid` also needs `sshd`,
`ssh-keygen`, `protoc`, `tic`, and a working `systemctl --user` session. The CI job
starts the runner's user manager before invoking that fixture.

For Linux UI tests, install the Desktop build dependencies plus `xvfb`, `xauth`,
`x11-utils`, `xdotool`, `xclip`, `openbox`, and Mesa Vulkan drivers:

```sh
cargo build --locked -p corvid-tui -p corvid-desktop -p corvid-agent-deck
xvfb-run -a python3 deploy/test_desktop.py --build target/debug
# Or validate an extracted release workflow's archive directory:
xvfb-run -a python3 deploy/test_desktop.py target/dist
```

To run the Linux Desktop title test over isolated loopback SSH, install OpenSSH
client/server and ensure `/run/sshd` exists, then run:

```sh
cargo build --locked -p corvid-tui
python3 deploy/test_title_ssh.py --binary target/debug/corvid
```

Pass `--release` and/or `--target <triple>` when using those Cargo build options.
The fixture creates private keys, client configuration, a temporary daemon and a
localhost-only SSH server under a temporary home subdirectory, then removes them.
It does not modify the user's SSH configuration or use an external SSH account.
The test prints input-to-title p50/p95/max latency for 32 changes. WAN latency,
packet loss, and the user's SSH configuration require separate environment checks.

## Remaining device qualification

Windows and macOS do not yet run the Linux Desktop keyboard/clipboard scenario.
Their native Rust tests and macOS window smoke test are narrower evidence.
WSL installation/reconnection still requires the explicitly ignored isolated-WSL
fixture. Real reboot and last-login tests need dedicated machines/accounts.
IME composition, accessibility permissions, notifications, multiple displays,
hardware GPU behavior, and physical Stream Deck USB input require separate
device qualification. Synthetic Unicode input does not validate an OS input
method editor.
