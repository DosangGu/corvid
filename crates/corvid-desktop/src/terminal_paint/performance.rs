use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AppContext, Application, Context, Render, TitlebarOptions, WindowBounds, WindowOptions, canvas,
    div, prelude::*,
};

use super::*;

const FRAMES: usize = 90;

struct Sample {
    prepare: Duration,
    paint: Duration,
    spans: usize,
}

struct Probe {
    parser: vt100::Parser,
    samples: Rc<RefCell<Vec<Sample>>>,
    tick: usize,
    preview: bool,
}

impl Render for Probe {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.tick += 1;
        self.parser
            .process(format!("\x1b[40;3HScreen update {}", self.tick).as_bytes());
        let screen = self.parser.screen().clone();
        let samples = Rc::clone(&self.samples);
        let preview = self.preview;
        if self.tick < FRAMES {
            window.request_animation_frame();
        }
        div().size_full().bg(rgb(BACKGROUND)).child(
            canvas(
                move |bounds, window, _| {
                    let started = Instant::now();
                    let metrics = Metrics::measure(window);
                    let frame = Frame::new(&screen, metrics.grid(bounds));
                    (frame, metrics, started.elapsed())
                },
                move |bounds, (frame, metrics, prepare), window, cx| {
                    let spans = frame.spans.len();
                    let started = Instant::now();
                    frame.paint(bounds, metrics, true, String::new(), window, cx);
                    let mut samples = samples.borrow_mut();
                    if samples.len() < FRAMES {
                        samples.push(Sample {
                            prepare,
                            paint: started.elapsed(),
                            spans,
                        });
                    }
                    if samples.len() == FRAMES && !preview {
                        cx.defer(|cx| cx.quit());
                    }
                },
            )
            .size_full(),
        )
    }
}

#[test]
#[ignore = "opens an isolated native window to measure terminal rendering"]
fn native_agent_redraw() {
    let samples = Rc::new(RefCell::new(Vec::new()));
    let result = Rc::clone(&samples);
    Application::new().run(move |cx| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Corvid rendering probe".into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(80.), px(80.)),
                size(px(1500.), px(1000.)),
            ))),
            ..Default::default()
        };
        cx.open_window(options, move |_, cx| {
            cx.new(|_| Probe {
                parser: synthetic_screen(),
                samples,
                tick: 0,
                preview: std::env::var_os("CORVID_RENDER_PROBE_PREVIEW").is_some(),
            })
        })
        .unwrap();
    });
    let samples = result.borrow();
    assert!(
        samples.len() >= FRAMES,
        "the native rendering probe was closed before it completed"
    );
    let mut totals: Vec<_> = samples
        .iter()
        .skip(5)
        .map(|sample| (sample.prepare + sample.paint).as_secs_f64() * 1000.)
        .collect();
    totals.sort_by(f64::total_cmp);
    eprintln!(
        "frames={} spans={} cold_ms={:.2} warm_p50_ms={:.2} warm_p95_ms={:.2} warm_max_ms={:.2}",
        samples.len(),
        samples[0].spans,
        (samples[0].prepare + samples[0].paint).as_secs_f64() * 1000.,
        totals[totals.len() / 2],
        totals[totals.len() * 95 / 100],
        totals.last().unwrap()
    );
}

fn synthetic_screen() -> vt100::Parser {
    let grid = Grid::new(45, 165);
    let mut parser = vt100::Parser::new(grid.rows, grid.cols, 0);
    for row in 0..45 {
        let line = if row % 4 == 0 {
            format!("\x1b[38;2;200;150;100m{}", "─".repeat(160))
        } else {
            format!(
                "\x1b[0m│ {row:02}  synthetic agent output: {}",
                "abc 한글 def e\u{301} 🐦 ".repeat(6)
            )
        };
        parser.process(format!("\x1b[{};1H{line}", row + 1).as_bytes());
    }
    parser.process(
        concat!(
            "\x1b[2;1H\x1b[1;3;4;38;2;240;180;120;48;2;40;60;80m",
            "Bold italic underline 한글 🐦\x1b[0m  plain text",
            "\x1b[7m inverse\x1b[0m"
        )
        .as_bytes(),
    );
    parser
}
