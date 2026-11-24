# Continuous integration coverage

## Pull requests

PR workflows run automatically when the target branch is `main`. On those
PRs, **Checks / Basics** verifies whitespace, Rust formatting, and workspace
version consistency. Component workflows also require a match for their
source, shared dependencies, or build inputs. A documentation-only PR to
`main` runs Basics.

| Workflow | Scope on a matching PR |
| --- | --- |
| **Checks / Core and daemon** | Linux Clippy and tests for async, core, wire protocol, daemon, and updater crates. Includes libghostty VT and history fixtures, the Compio-only protocol build, and a dependency graph check for Tokio. |
| **Checks / TUI** | Linux client and companion tests, scripted peer attach, and integration tests using one standalone daemon build for IPC, PTY routing and relay, reconnect, and workspace and monitor endpoints. |
| **Checks / Desktop** | Linux Desktop unit tests and scripted IPC tests for inventory, render state, and attach negotiation. |
| **Checks / Agent Deck** | Linux Agent Deck, Deck core, and presentation tests. |
| **Checks / UI** | Desktop and Agent Deck smoke tests on all five release targets when the UI test harness changes. Also supports manual runs by platform and case. |
| **Checks / Packaging** | Shared icons, manifest and packaging fixtures, and installer shell syntax. |

Workspace manifest, lockfile, and Rust toolchain changes run the component
workflows. GitHub does not create component workflow runs for PRs outside their
path filters. Basics runs on every PR to `main` and has a stable job name if
the repository elects to require it there. Workflows with `workflow_dispatch`
can be launched manually after they are available on `main`, including on
branches used by stacked PRs.

GitHub's native stack feature evaluates workflow branch filters against the
stack's trunk for every entry. To run CI only as each PR reaches `main`,
keep each PR based on the preceding branch and retarget it after that PR merges.
See [GitHub's stack CI documentation](https://docs.github.com/en/pull-requests/how-tos/merge-and-close-pull-requests/optimizing-ci-for-stacked-pull-requests).

### Hosted macOS runner cancellations

If GitHub cancels a `macos-latest` job before assigning a runner, check the
job annotation for limited Apple Silicon capacity. Once the workflow attempt
has completed, rerun the affected job by its ID. GitHub retains the completed
matrix results and also reruns any jobs that depend on the selected job.

```sh
gh run rerun --repo DosangGu/corvid --job <job-id>
```

Record the job link, runner label, and queue duration in [issue #399](https://github.com/DosangGu/corvid/issues/399).

## Release qualification

Update `deploy/release-notes.md` in the version preparation PR to `main` to
trigger **Checks / Release**. The same workflow can be dispatched manually to
validate packaging or runtime changes. It builds and tests release packages
for Linux and Windows x64 and ARM64 and for Apple Silicon macOS, and verifies
Cargo source packages on Linux, Windows, and macOS.

The workflows use `ubuntu-latest`, `windows-latest`, and `macos-latest` for the
default images. Linux and Windows ARM64 use `ubuntu-26.04-arm` and
`windows-11-arm`. Release publication builds the assets from the tag with
the same targets. Each installer job must pass the packaged UI smoke suite
before GitHub Release publication. See [build and publish](05_release_distribution.md#build-and-publish).

| Platform | Automated behavior |
| --- | --- |
| All five targets | Runtime and workspace Rust tests, Clippy, and native builds. Tests cover protocol framing, identities, input fencing, and restoration through the daemon/client boundary. |
| Windows | `windows_daemon.rs` runs real ConPTY sessions and private named pipes, including input/paste isolation, tab naming, detach/resume, multiple writers, provider observations, and daemon handoff. Daemon and client tests cover PowerShell history and delivery of queued events after pipe closure. |
| Windows and macOS | The runtime jobs verify the Compio-only protocol build and check the native workspace dependency graph for Tokio. The macOS runtime tests exercise agent recovery with a delayed Claude confirmation. |
| Linux and macOS | The terminal spike and `interactive.py --tty-only` exercise real PTYs: fragmented UTF-8, escape sequences, bracketed paste, terminal queries, resize, Ctrl-C, exit status, TTY restoration, pairing, tabs, and reattachment to the same child. Linux runs the full interactive suite instead of the TTY-only subset. |
| Linux | The full interactive suite also checks automatic daemon startup, shell continuity across replacement, and synthetic agent notifications. The loopback OpenSSH fixture checks abrupt disconnects, companion resynchronization, and same-PTY reattachment under a transient systemd user service. |
| All five Desktop targets | The packaged UI smoke suite opens Desktop, connects to the fixture daemon, and opens an Agent Deck window. |
| Linux, Windows x64, and macOS Desktop | The packaged smoke suite pastes synthetic Unicode through the system clipboard and checks the receiving shell. Linux and Windows x64 also check tab isolation. |
| Linux Desktop | Xvfb/Openbox tests drive the real application with keyboard and clipboard events. They check Unicode paste, tab creation and selection, independent windows, the new-window menu, settings, close/quit/relaunch, saved window count, and retained shell variables. |

The Python integration drivers run as explicit workflow steps. UI tests check
observable state and exact synthetic output using a private profile and a
temporary daemon. Linux creates a separate X11 display for each case.
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
and daemon lifecycle on all three platforms. Windows validates executable icons and Setup
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

### Desktop and Agent Deck UI

Desktop and Agent Deck UI scenarios and drivers live in
`tests/ui/linux/`, `tests/ui/macos/`, and `tests/ui/windows/`.
The platform directories also contain the Linux SSH and the macOS Dock and
DMG scenarios. Their shared monitor fixture lives in `tests/ui/`.
The plugin manifest registers the Windows Deck tray integration test from
`plugins/agent-deck/tests/ui/windows_tray.rs`. The TUI PTY suite lives
alongside the other integration tests in `crates/corvid-tui/tests/`.

Build the executables and run the smoke suite from the checkout. Use `python3`
where the platform installs Python under that name.

```sh
cargo build --locked -p corvid-tui -p corvid-desktop -p corvid-agent-deck-ui
python tests/ui/run.py --list
python tests/ui/run.py --build target/debug --suite smoke
python tests/ui/run.py --build target/debug --case desktop-paste
python tests/ui/run.py --assets target/dist --suite smoke
```

`--assets` takes a release asset directory containing the Linux Desktop archive,
Windows Desktop ZIP, or macOS DMG for the current platform. `--suite full`
includes Linux window and Deck focus tests for a build, the Windows Deck tray
test, and the macOS DMG verification when a DMG is supplied. The Windows tray
case compiles the Rust integration test and uses the selected Deck executable.
Run it in a session with a Windows notification area to verify hiding, reopening,
and pinning the Deck. The hosted ARM64 runner did not expose the Deck tray window.

- Linux needs the Desktop build dependencies plus Xvfb, xauth, X11 utilities,
  xdotool, xclip, Openbox, scrot, and a Vulkan driver such as Mesa. The Deck
  focus case also needs OpenSSH client and server and a writable test home.
- Windows smoke tests verify Desktop and Deck windows. They also check tab
  isolation and paste on x64. Run `--case desktop-paste` on an unlocked
  interactive ARM64 desktop with a US input source for the same input check.
  The hosted ARM64 runner refused foreground activation during qualification.
- macOS needs a logged-in GUI session, Xcode command line tools, and
  Accessibility permission to post key events. The runner wraps a local Deck
  build in a temporary signed app bundle. Paste tests use the system
  clipboard and foreground window. Use a dedicated session if other
  applications need those resources.

In GitHub Actions, run **Checks / UI** to select a platform and optional case.
Failures report the scenario in the job log and save available screenshots and
process logs under `target/ui-tests/`.

### SSH title synchronization

To run the Linux Desktop title test over isolated loopback SSH, install OpenSSH
client/server and ensure `/run/sshd` exists, then run:

```sh
cargo build --locked -p corvid-tui
python3 tests/ui/linux/test_title_ssh.py --binary target/debug/corvid
```

Pass `--release` and/or `--target <triple>` when using those Cargo build options.
The fixture creates private keys, client configuration, a temporary daemon and a
localhost-only SSH server under a temporary home subdirectory, then removes them.
It does not modify the user's SSH configuration or use an external SSH account.
The test prints input-to-title p50/p95/max latency for 32 changes. WAN latency,
packet loss, and the user's SSH configuration require separate environment checks.

## Remaining device qualification

WSL installation/reconnection still requires the explicitly ignored isolated-WSL
fixture. Real reboot and last-login tests need dedicated machines/accounts.
IME composition, accessibility permissions, notifications, multiple displays,
and hardware GPU behavior require separate device qualification. Synthetic
Unicode input does not validate an OS input method editor.

## WSL 2 lifecycle check

`Checks / WSL 2` first boots an isolated Alpine distribution on a Windows
runner. A runner without a bootable WSL 2 environment reports a skipped
lifecycle in the workflow summary. On a WSL 2 Windows host, the same check
can reuse a trusted Linux rootfs archive and matching built artifacts.

For a stacked branch, dispatch `checks-release.yml` with `scope=wsl` to run
only WSL qualification. That invocation requires a bootable WSL 2 runner.

```powershell
python deploy/test_wsl.py --rootfs rootfs.tar.gz --desktop-test corvid_desktop_test.exe --daemon corvidd
```

Build the Windows test executable with `cargo test --locked --release -p corvid-desktop
--bin corvid-desktop --no-run`. Supply a release-build Linux musl daemon for
the distribution architecture with libghostty-vt. The harness verifies
managed install consent, then opens a PTY and checks that the shell survives a
daemon upgrade. It unregisters the temporary distribution it imported.
