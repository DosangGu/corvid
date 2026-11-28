- Adopt the **Neutral Obsidian** UI palette across Corvid Desktop, Agent Deck, and default terminal colors, and replace the application and tray icons with the **Geometric Wing** mark. [#559](https://github.com/DosangGu/corvid/pull/559)
- Replace the `view_state` `Recorder` mutex with a bounded lockless `mpsc` worker actor to preserve debounced batch persistence without in-memory lock contention. [#557](https://github.com/DosangGu/corvid/pull/557)

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.10.0/docs/guides/release-and-install.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.9.0...v0.10.0)
