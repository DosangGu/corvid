Corvid v0.4.3 completes the switch to Compio across the daemon, command line, Desktop, and Agent Deck.

- Local IPC, SSH and WSL relays, and terminal attachment use Compio in standard builds. [#284](https://github.com/DosangGu/corvid/pull/284) · [#285](https://github.com/DosangGu/corvid/pull/285)
- Companion monitoring, Desktop workspace inventory, and Agent Deck focus routing use the same runtime. [#286](https://github.com/DosangGu/corvid/pull/286) · [#288](https://github.com/DosangGu/corvid/pull/288)
- The preview feature flags have been removed. CI checks the Linux, macOS, and Windows dependency graphs for Tokio. [#289](https://github.com/DosangGu/corvid/pull/289)

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.4.3/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.4.2...v0.4.3)
