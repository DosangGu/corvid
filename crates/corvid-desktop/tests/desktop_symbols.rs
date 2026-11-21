#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[path = "../src/desktop_font.rs"]
mod desktop_font;

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use gpui::{
    Font, ForegroundDependency, GlyphRenderMode, LineLayout, Pixels, PlatformTextSystem,
    RasterStyleRequest, RenderGlyphParams, TextRun, WindowTextSystem, font, px, rgb,
};

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
    let platform = gpui_platform::current_platform(true);
    let application = gpui::Application::with_platform(platform.clone());
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
    verify_title_presentation(&shaping, platform.text_system().as_ref())?;
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
fn verify_title_presentation(
    shaping: &WindowTextSystem,
    platform_text: &dyn PlatformTextSystem,
) -> anyhow::Result<()> {
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
            if title.starts_with('✳') {
                let offset = rasterized_symbol_center(platform_text, &line)?;
                println!("{title:?} at {zoom}×: center offset {offset:.2} px");
                if cfg!(target_os = "macos") && !expected_color {
                    assert!(offset.abs() <= 1.5, "Misaligned text symbol in {title:?}");
                }
            }
        }
    }
    println!("PASS Desktop: default, text and emoji presentation in tab titles");
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn rasterized_symbol_center(
    platform_text: &dyn PlatformTextSystem,
    line: &LineLayout,
) -> anyhow::Result<f32> {
    let run = &line.paint_fragments[0];
    let glyph = &run.glyphs[0];
    let requested_mode = if glyph.is_emoji {
        GlyphRenderMode::Color
    } else {
        GlyphRenderMode::Grayscale
    };
    let raster_style = platform_text.prepare_raster_style(RasterStyleRequest {
        font_id: run.font_id,
        glyph_id: glyph.id,
        scene_color: rgb(0x83dec0),
        requested_mode,
        foreground_dependency: ForegroundDependency::Full,
    });
    let raster = platform_text.rasterize_glyph(&RenderGlyphParams {
        font_id: run.font_id,
        glyph_id: glyph.id,
        font_size: run.font_size,
        subpixel_variant: Default::default(),
        scale_factor: 1.,
        raster_style,
    })?;
    raster.validate()?;
    assert!(!raster.pixels.is_empty(), "Missing raster for title symbol");

    // GPUI places the baseline at (ascent - descent) / 2 from the center of
    // the line. The tab button adds the same padding above and below it.
    let baseline = f32::from((line.ascent - line.descent) / 2.);
    let center = raster.bounds.origin.y.0 as f32 + raster.size.height.0 as f32 / 2.;
    Ok(baseline + f32::from(glyph.position.y) + center)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn main() {}
