#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[path = "../src/desktop_font.rs"]
mod desktop_font;

// AppKit initializes on the main thread.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn main() -> anyhow::Result<()> {
    use gpui::{Font, TextRun, WindowTextSystem, font, px};

    let application = gpui_platform::headless();
    let text = application.text_system();
    desktop_font::register(&text)?;
    let shaping = WindowTextSystem::new(text);
    let default_mono = if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(windows) {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    };
    let shape = |sample: &str, font: Font| {
        shaping.layout_line(
            sample,
            px(15.),
            &[TextRun {
                len: sample.len(),
                font,
                ..Default::default()
            }],
        )
    };
    for family in [default_mono, ".SystemUIFont"] {
        let preferred = shape("M", font(family));
        let with_fallback = desktop_font::with_symbols(family);
        let latin = shape("M", with_fallback.clone());
        assert_eq!(
            latin.paint_fragments[0].font_id,
            preferred.paint_fragments[0].font_id
        );
        assert_eq!(latin.width, preferred.width);

        for symbol in ["⎾", "⎿", "⏵", "⏸", "⏺", "▏", "☐"] {
            let line = shape(symbol, with_fallback.clone());
            let mut glyphs = line
                .paint_fragments
                .iter()
                .flat_map(|fragment| fragment.glyphs.iter());
            assert!(
                glyphs.any(|glyph| glyph.id.0 != 0),
                "{family} lost the glyph for {symbol}"
            );
        }
    }
    println!("PASS Desktop: regular text and technical symbol fallback");
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn main() {}
