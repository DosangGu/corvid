# Desktop text fallback backport

`fontique` and `parley` start from the crates.io 0.11.1 sources used by GPUI CE 0.3.0. Both directories include the original upstream licenses. The workspace patches them for the lifetime of [Corvid #291](https://github.com/DosangGu/corvid/issues/291).

Fontique uses the platform's fallback query with the actual cluster after the configured and script-based font families have been tried. Parley gives the emoji family priority for an explicit emoji request or emoji-default character. Text-default clusters and explicit text requests try the text and system fallbacks before the generic emoji family. The original string and variation selectors still reach the shaper.

The backport draws on [Parley #689](https://github.com/linebender/parley/pull/689) and [Parley #692](https://github.com/linebender/parley/pull/692). When released dependencies cover the cases in `desktop_symbols`, remove these directories and their root Cargo patches in the dependency update. Evaluate the shipped technical-symbol font separately.
