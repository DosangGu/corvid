# Taffy 0.9.0 in Corvid

The source, examples, and upstream README come from the
[Taffy 0.9.0 crates.io archive](https://crates.io/crates/taffy/0.9.0).
The archive SHA-256 is
`a13e5d13f79d558b5d353a98072ca8ca0e99da429467804de959aa8c83c9a004`.
The MIT license comes from the
[upstream source revision](https://github.com/DioxusLabs/taffy/blob/599b1b7d7410d97e13bafc4c01fbe6d7a9cf060a/LICENSE.md).

GPUI 0.2.2 pins Taffy 0.9.0. The manifest in this copy requires
`grid` 1.0.1 or later within the 1.x series to address
[CVE-2026-42199](https://github.com/advisories/GHSA-38c5-483c-4qqp).
[Taffy's upstream dependency update](https://github.com/DioxusLabs/taffy/pull/864)
appeared in Taffy 0.9.2. Cargo uses this copy for Corvid workspace builds.
Remove the copy and the workspace patch when GPUI selects a compatible Taffy
release with a patched `grid` or without the `grid` dependency.

Desktop and Agent Deck binaries built from the Corvid workspace use this
copy. Standalone crates.io builds of the Agent Deck UI resolve the published
GPUI dependency graph. The `corvid-tui` crate uses Agent Deck's headless library.
