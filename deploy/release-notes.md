Corvid v0.3.1 improves Desktop terminal interaction, workspace creation, and Agent Deck status.

- Scroll through a fullscreen agent’s conversation when it enables terminal mouse reporting. At a shell prompt, scrolling up opens earlier terminal output, and scrolling back to the bottom resumes live input. Desktop also keeps older terminal rows available in history. [#144](https://github.com/DosangGu/corvid/pull/144)
- Desktop forwards key combinations it does not reserve, so terminal apps can distinguish Shift+Enter from Enter. [#142](https://github.com/DosangGu/corvid/pull/142)
- Desktop stays responsive when a terminal produces a burst of output. [#145](https://github.com/DosangGu/corvid/pull/145)
- Create a workspace from the **+** beside a local, SSH, or WSL machine in the workspace picker. The folder chooser starts in that machine's home directory. [#141](https://github.com/DosangGu/corvid/pull/141)
- Agent Deck shows when a monitored Claude or Codex session receives a prompt and when it observes a response. [#143](https://github.com/DosangGu/corvid/pull/143)

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation. When connecting to an older managed SSH host or WSL distribution, accept the offered daemon update.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.3.1/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.3.0...v0.3.1)
