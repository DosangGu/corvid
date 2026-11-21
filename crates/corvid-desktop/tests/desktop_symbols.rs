#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[path = "../src/desktop_font.rs"]
mod desktop_font;

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use gpui::{Font, LineLayout, Pixels, TextRun, WindowTextSystem, font, px};

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn shape(
    shaping: &WindowTextSystem,
    sample: &str,
    font: Font,
    size: Pixels,
) -> std::sync::Arc<LineLayout> {
    shaping.layout_line(
        sample,
        size,
        &[TextRun {
            len: sample.len(),
            font,
            ..Default::default()
        }],
    )
}

// AppKit initializes on the main thread.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn main() -> anyhow::Result<()> {
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
    for family in [default_mono, ".SystemUIFont"] {
        let preferred = shape(&shaping, "M", font(family), px(15.));
        let with_fallback = desktop_font::with_symbols(family);
        let latin = shape(&shaping, "M", with_fallback.clone(), px(15.));
        assert_eq!(
            latin.paint_fragments[0].font_id,
            preferred.paint_fragments[0].font_id
        );
        assert_eq!(latin.width, preferred.width);

        for symbol in ["⎾", "⎿", "⏵", "⏸", "⏺", "▏", "☐"] {
            let line = shape(&shaping, symbol, with_fallback.clone(), px(15.));
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
    verify_title_presentation(&shaping);
    #[cfg(target_os = "macos")]
    {
        // The installed Zapf Dingbats covers this character. It is absent from
        // the app's technical-symbol stack and Fontique's script fallback.
        let line = shape(&shaping, "❯", font(".SystemUIFont"), px(13.));
        assert_ne!(line.paint_fragments[0].glyphs[0].id.0, 0);
    }
    println!("PASS Desktop: regular text and technical symbol fallback");
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn verify_title_presentation(shaping: &WindowTextSystem) {
    let font = desktop_font::with_symbols(".SystemUIFont");
    for zoom in [1., 1.4] {
        for (title, expected_color) in [
            ("✳ Claude Code", false),
            ("✳\u{fe0e} Claude Code", false),
            ("✳\u{fe0f} Claude Code", true),
            ("Notes ❤\u{fe0e} title", false),
            ("Notes ❤\u{fe0f} title", true),
            ("Manual title 🟢", true),
            ("Digits 123 # *", false),
        ] {
            let line = shape(shaping, title, font.clone(), px(13. * zoom));
            let glyphs = line
                .paint_fragments
                .iter()
                .flat_map(|run| run.glyphs.iter());
            let mut color = 0;
            for glyph in glyphs {
                assert_ne!(glyph.id.0, 0, "Missing glyph in {title:?} at {zoom}×");
                color += usize::from(glyph.is_emoji);
            }
            assert_eq!(
                color,
                usize::from(expected_color),
                "Unexpected presentation in {title:?} at {zoom}×",
            );
        }
    }
    println!("PASS Desktop: default, text and emoji presentation in tab titles");
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn main() {}
