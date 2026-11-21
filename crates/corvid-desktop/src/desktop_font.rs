use std::borrow::Cow;
use std::sync::OnceLock;

use gpui::{Font, FontFallbacks, SharedString, TextSystem, font};

const SYMBOL_FAMILY: &str = "STIX Two Math";
const SYMBOL_FONT: &[u8] = include_bytes!("../../../assets/fonts/STIXTwoMath-Regular.otf");

pub fn register(text: &TextSystem) -> anyhow::Result<()> {
    text.add_fonts(vec![Cow::Borrowed(SYMBOL_FONT)])
}

pub fn with_symbols(family: impl Into<SharedString>) -> Font {
    static FALLBACKS: OnceLock<FontFallbacks> = OnceLock::new();
    let fallbacks = FALLBACKS.get_or_init(|| {
        let system_symbols = if cfg!(target_os = "macos") {
            "Apple Symbols"
        } else if cfg!(windows) {
            "Segoe UI Symbol"
        } else {
            "DejaVu Sans"
        };
        FontFallbacks::from_fonts(vec![SYMBOL_FAMILY.to_owned(), system_symbols.to_owned()])
    });

    let mut font = font(family);
    font.fallbacks = Some(fallbacks.clone());
    font
}
