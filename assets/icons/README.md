# Corvid application icon

`corvid.svg` is the canonical application artwork. `corvid.png` is the 256-pixel
RGBA image; `corvid.ico` contains 16, 20, 24, 32, 48, 64, 128, and 256-pixel images.
Regenerate the application and Agent Deck tray assets from the workspace root:

```sh
cargo run -p corvid-agent-deck --example render-icons
```

Windows embeds resource 1 in Desktop, Deck, the terminal executable, Agent Deck,
and all three stable launchers. GPUI uses it for every native window, including
Settings. Setup and shortcuts also use the ICO. Existing installed launchers
retain the v1 installation contract and are not replaced while they may be in use.

Desktop uses `Corvid.Desktop` for its Windows application identity and Linux
window class. Linux windows set `_NET_WM_ICON` directly, including portable and
development launches. The Linux installer registers Desktop and the bundled
Agent Deck in the user application menu and links icons into the hicolor theme.
Those entries reference `current`, so version changes do not leave stale paths.
Desktop's About page and Agent Deck's window use the shared PNG.

Agent Deck's normal and attention tray icons remain in
`plugins/agent-deck/assets`; they are separate monochrome status artwork.
