# Corvid compatibility patch

This directory contains the source, tests, README, and licenses from the
crates.io `xattr` 0.2.3 release. `Cargo.toml` is the release's original
`Cargo.toml.orig`, with an empty `[workspace]` table to keep standalone test
runs independent of enclosing worktrees.

Source: <https://crates.io/crates/xattr/0.2.3>

Release archive SHA-256:
`6d1526bbe5aaeb5eb06885f4d987bcdfa5e23187055de9b83fe00156a821fabc`.

GPUI 0.2.2 depends on `gpui_http_client`, which depends on `zed-async-tar`
0.5.0-zed and enables `xattr` 0.2.3. That version refers to `libc::ENOATTR`,
which libc 0.2.190 removed on Linux and Android.

The only source change is in `src/sys/mod.rs`: Linux and Android use
`libc::ENODATA` for a missing extended attribute. macOS and BSD retain
`libc::ENOATTR`. The public API and filesystem operations are unchanged.

The root manifest patches crates.io and excludes this dependency from workspace
membership. Its separate lockfile uses libc 0.2.191 to exercise compatibility
even before the workspace upgrades libc. Linux CI runs its upstream tests with
`cargo test --locked --release --target x86_64-unknown-linux-gnu --manifest-path vendor/xattr/Cargo.toml`.
The path and file-descriptor tests cover missing attributes, set/get/list/remove
operations, and missing files.

Remove this directory and the root patch when GPUI's published dependency tree
no longer requires the incompatible `xattr` release. Keep the upstream MIT and
Apache-2.0 licenses with this copy.
