Agent Deck now shows active agent runs across recognized local and remote hosts, including runs without an assigned slot. Select a card to focus its terminal in Desktop. [Agent Deck changes](https://github.com/DosangGu/corvid/pull/404)

- The mini and expanded decks show the same agents in the same order. Runs beyond the selected grid's capacity appear in the queue, and cards show their host when multiple hosts are present. [Current runs in Companion](https://github.com/DosangGu/corvid/pull/403) · [Agent Deck guide](https://github.com/DosangGu/corvid/blob/v0.6.1/docs/06_agent_deck.md#slots-and-queue)
- The WSL 2 lifecycle check retries slow boots, and the relay EOF test uses a native child process for more reliable coverage. [WSL boot probe](https://github.com/DosangGu/corvid/pull/408) · [Relay EOF test](https://github.com/DosangGu/corvid/pull/405)

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.6.1/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.6.0...v0.6.1)
