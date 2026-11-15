// AppKit must initialize on the process main thread, not a Rust test worker.
#[cfg(target_os = "macos")]
fn main() {
    use gpui::{Application, font, px};

    let application = Application::headless();
    let text = application.text_system();
    let ui = text.resolve_font(&font(".SystemUIFont"));
    let narrow = text.advance(ui, px(15.), 'i').unwrap().width;
    let wide = text.advance(ui, px(15.), 'W').unwrap().width;
    // GPUI's fallback text system invents uniform metrics but paints no glyphs.
    assert!(wide > narrow, "macOS must use its native text backend");

    let terminal = text.resolve_font(&font("Menlo"));
    let narrow = text.advance(terminal, px(15.), 'i').unwrap().width;
    let wide = text.advance(terminal, px(15.), 'W').unwrap().width;
    assert_eq!(narrow, wide);
    println!("PASS macOS: native UI fonts and fixed-width terminal font");
}

#[cfg(not(target_os = "macos"))]
fn main() {}
