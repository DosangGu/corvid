#[cfg(target_os = "macos")]
#[path = "../src/terminal_fonts.rs"]
mod terminal_fonts;

// AppKit must initialize on the process main thread, not a Rust test worker.
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use gpui::{Application, font, px};
    use terminal_fonts::{DEFAULT_FONT, FONT_SIZE};

    let application = Application::headless();
    let text = application.text_system();
    let ui = text.resolve_font(&font(".SystemUIFont"));
    let narrow = text.advance(ui, px(15.), 'i').unwrap().width;
    let wide = text.advance(ui, px(15.), 'W').unwrap().width;
    // GPUI's fallback text system invents uniform metrics but paints no glyphs.
    assert!(wide > narrow, "macOS must use its native text backend");

    assert_eq!(DEFAULT_FONT, "Menlo");
    let terminal = text.resolve_font(&font(DEFAULT_FONT));
    let narrow = text.advance(terminal, FONT_SIZE, 'i')?.width;
    let wide = text.advance(terminal, FONT_SIZE, 'W')?.width;
    assert_eq!(narrow, wide);
    verify_preferences(&text)?;
    println!("PASS macOS: native fonts, Menlo default, saved selection and missing-font fallback");
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_preferences(text: &gpui::TextSystem) -> anyhow::Result<()> {
    use terminal_fonts::{DEFAULT_FONT, TerminalFonts};

    let state = tempfile::tempdir()?;
    let mut fonts = TerminalFonts::load(state.path(), text);
    assert_eq!(fonts.current().as_str(), DEFAULT_FONT);
    assert_eq!(fonts.default_family().as_str(), DEFAULT_FONT);
    assert!(fonts.selected().is_none());
    let alternate = fonts
        .available()
        .iter()
        .find(|family| family.as_str() != DEFAULT_FONT)
        .expect("macOS provides additional monospaced fonts")
        .clone();

    fonts.select(Some(alternate.clone()))?;
    let mut restored = TerminalFonts::load(state.path(), text);
    assert_eq!(restored.current(), &alternate);
    assert_eq!(restored.selected(), Some(&alternate));
    restored.select(None)?;
    assert_eq!(
        TerminalFonts::load(state.path(), text).current().as_str(),
        DEFAULT_FONT
    );

    std::fs::write(
        state.path().join("desktop-preferences/terminal.json"),
        r#"{"font_family":"Corvid Missing Font Fixture"}"#,
    )?;
    let fallback = TerminalFonts::load(state.path(), text);
    assert_eq!(fallback.current().as_str(), DEFAULT_FONT);
    assert!(fallback.selected().is_none());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {}
