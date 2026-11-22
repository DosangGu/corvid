#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[path = "../src/desktop_font.rs"]
mod desktop_font;

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[path = "../src/workspace_view/title.rs"]
mod tab_title;

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
mod fixture {
    use std::cell::RefCell;
    use std::rc::Rc;

    use anyhow::{Result, ensure};
    use gpui::{
        Application, Bounds, Context, Font, IntoElement, LineLayout, Pixels, Render, TextRun,
        TextStyle, TextSystem, Window, WindowBounds, WindowOptions, WindowTextSystem, div, font,
        prelude::*, px, rgb, size,
    };

    use super::{desktop_font, tab_title};

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
                color: rgb(0xffffff).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        )
    }

    fn verify_symbols(shaping: &WindowTextSystem, text: &TextSystem) -> Result<()> {
        let default_mono = if cfg!(target_os = "macos") {
            "Menlo"
        } else if cfg!(windows) {
            "Consolas"
        } else {
            "DejaVu Sans Mono"
        };
        for family in [default_mono, ".SystemUIFont"] {
            let preferred = shape(shaping, "M", font(family), px(15.));
            let with_fallback = desktop_font::with_symbols(family);
            let latin = shape(shaping, "M", with_fallback.clone(), px(15.));
            ensure!(
                latin.width == preferred.width,
                "The symbol fallback changed {family}'s Latin advance"
            );

            for symbol in ['⎾', '⎿', '⏵', '⏸', '⏺', '▏', '☐'] {
                let line = shape(shaping, &symbol.to_string(), with_fallback.clone(), px(15.));
                ensure!(
                    line.runs.iter().any(|run| {
                        !run.glyphs.is_empty()
                            && text
                                .typographic_bounds(run.font_id, px(15.), symbol)
                                .is_ok()
                    }),
                    "{family} lost the glyph for {symbol}",
                );
            }
        }
        println!("PASS Desktop: regular text and technical symbol fallback");
        Ok(())
    }

    fn verify_title_presentation(shaping: &WindowTextSystem) -> Result<()> {
        let font = desktop_font::with_symbols(".SystemUIFont");
        let style = TextStyle {
            font_family: font.family,
            font_fallbacks: font.fallbacks,
            ..Default::default()
        };
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
                let runs = tab_title::emoji_presentation_runs(title, &style)
                    .unwrap_or_else(|| vec![style.to_run(title.len())]);
                let lines = shaping.shape_text(title.into(), px(13. * zoom), &runs, None, None)?;
                let color = lines[0]
                    .unwrapped_layout
                    .runs
                    .iter()
                    .flat_map(|run| &run.glyphs)
                    .any(|glyph| glyph.is_emoji);
                let expects_native_emoji_fallback =
                    cfg!(target_os = "linux") && title.contains('\u{fe0f}');
                ensure!(
                    expects_native_emoji_fallback || color == expected_color,
                    "Unexpected presentation in {title:?} at {zoom}×",
                );
            }
        }
        println!("PASS Desktop: native title and emoji presentation");
        Ok(())
    }

    struct TabTitles;

    impl Render for TabTitles {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .font(desktop_font::with_symbols(".SystemUIFont"))
                .flex()
                .flex_col()
                .gap(px(2.))
                .children([px(500.), px(90.)].map(|max_width| {
                    div().flex().gap(px(2.)).children(
                        [
                            ("✳️ Claude Code planning the next release", Some("!")),
                            ("A long plain workspace title", Some("▾")),
                            ("🧑🏽‍💻 개발", None),
                        ]
                        .map(|(title, suffix)| {
                            let button = div()
                                .id(title)
                                .flex_none()
                                .max_w(max_width)
                                .text_size(px(13.))
                                .whitespace_nowrap();
                            tab_title::button(button, title.to_owned(), suffix)
                        }),
                    )
                }))
        }
    }

    pub(super) fn run() -> Result<()> {
        let application = Application::new();
        desktop_font::register(&application.text_system())?;
        let result = Rc::new(RefCell::new(Ok(())));
        let window_result = result.clone();

        application.run(move |cx| {
            let shaping_result = window_result.clone();
            let opened = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1200.), px(100.)),
                        cx,
                    ))),
                    ..Default::default()
                },
                move |window, cx| {
                    let shaping = window.text_system();
                    *shaping_result.borrow_mut() = verify_symbols(shaping, cx.text_system())
                        .and_then(|()| verify_title_presentation(shaping));
                    cx.new(|_| TabTitles)
                },
            );
            cx.spawn(async move |cx| {
                let checked = window_result.replace(Ok(()));
                let painted = cx.update(|cx| {
                    let result = opened.and_then(|handle| {
                        cx.update_window(handle.into(), |_, window, cx| {
                            for _ in 0..2 {
                                window.refresh();
                                let _ = window.draw(cx);
                            }
                        })
                    });
                    cx.quit();
                    result
                });
                *window_result.borrow_mut() = checked.and(painted.flatten());
            })
            .detach();
        });

        result.replace(Ok(()))?;
        println!("PASS Desktop: painted mixed and plain tab titles");
        Ok(())
    }
}

// AppKit initializes on the main thread.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn main() -> anyhow::Result<()> {
    fixture::run()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn main() {}
