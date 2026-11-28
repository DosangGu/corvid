# Provider marks

- `provider-claude.svg`: Claude mark from [Simple Icons 15.0.0](https://github.com/simple-icons/simple-icons/blob/15.0.0/icons/claude.svg) with Anthropic's Claude terracotta brand fill (`#D97757`) and an explicit `128x128` viewport.
- `provider-codex.svg`: OpenAI mark identifying the Codex provider, from [Simple Icons 15.0.0](https://github.com/simple-icons/simple-icons/blob/15.0.0/icons/openai.svg).
- `provider-antigravity.svg`: Official color Antigravity mark sourced from [Lobe Icons](https://github.com/lobehub/lobe-icons) (`antigravity-color.svg`) with an explicit `128x128` viewport.
- `provider-agent.svg`: original generic terminal glyph for unknown providers.

Simple Icons distributes its unmodified SVGs under [CC0 1.0](https://github.com/simple-icons/simple-icons/blob/15.0.0/LICENSE.md), and Lobe Icons distributes its icon package under MIT. Regardless of icon-collection packaging, the underlying brand marks remain the property of their respective owners (see [THIRD_PARTY_NOTICES.md](../../../THIRD_PARTY_NOTICES.md)), are excluded from this repository's Apache-2.0 license grant, identify the observed provider, and do not imply endorsement.

GPUI renders color provider marks (`Claude`, `Antigravity`) via `gpui::img()` (`resvg` full-color rasterization) and monochrome marks (`Codex`, `Generic`) via `gpui::svg()` tinted to the card text color. Status colors belong to the surrounding card.
