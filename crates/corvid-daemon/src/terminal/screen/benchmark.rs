use std::hint::black_box;
use std::time::{Duration, Instant};

use anyhow::{Context as _, ensure};
use prost::Message as _;

use super::*;

const TURNS: u16 = 450;
const FRAGMENT: usize = 79;

#[test]
#[ignore = "run with spikes/terminal/benchmark_engines.py"]
fn measure_engine() -> Result<()> {
    let profile = std::env::var("CORVID_BENCH_PROFILE")?;
    let sessions: usize = std::env::var("CORVID_BENCH_SESSIONS")?.parse()?;
    ensure!(sessions > 0 && sessions <= 128, "invalid session count");
    let viewport = Viewport::new(100, 30).context("benchmark viewport")?;
    let trace: Vec<_> = (0..TURNS).map(|turn| output(&profile, turn)).collect();
    let rss_before = current_rss();

    let mut terminals = Vec::with_capacity(sessions);
    let mut parse_time = Duration::ZERO;
    let mut parsed_bytes = 0;
    for _ in 0..sessions {
        let mut terminal = TerminalScreen::new(viewport)?;
        let start = Instant::now();
        for turn in &trace {
            for fragment in turn.as_bytes().chunks(FRAGMENT) {
                black_box(terminal.process(fragment)?);
                parsed_bytes += fragment.len();
            }
        }
        parse_time += start.elapsed();
        terminals.push(terminal);
    }
    let idle_rss = current_rss();
    let terminal = terminals.first_mut().unwrap();
    let mut full_time = Duration::ZERO;
    let mut diff_time = Duration::ZERO;
    let mut full_bytes = 0;
    let mut diff_bytes = 0;
    let mut previous = terminal.snapshot()?;
    for (index, turn) in trace.iter().enumerate() {
        for fragment in turn.as_bytes().chunks(FRAGMENT) {
            black_box(terminal.process(fragment)?);
        }
        // Use a fresh dirty screen for each timed frame. Alternate ordering to
        // avoid giving one path all the benefit from the projection cache.
        let start = Instant::now();
        if index % 2 == 0 {
            full_bytes += terminal.render(None, viewport, None)?.len();
            previous = terminal.snapshot()?;
            full_time += start.elapsed();
        } else {
            diff_bytes += terminal.render(Some(&previous), viewport, None)?.len();
            previous = terminal.snapshot()?;
            diff_time += start.elapsed();
        }
    }
    let history_bytes = black_box(terminal.history()?.encoded_len());
    black_box(&terminals);
    println!(
        "CORVID_BENCH {}",
        serde_json::json!({
            "engine": "ghostty",
            "profile": profile,
            "sessions": sessions,
            "parsed_bytes": parsed_bytes,
            "parse_ms": parse_time.as_secs_f64() * 1000.0,
            "full_ms": full_time.as_secs_f64() * 1000.0,
            "diff_ms": diff_time.as_secs_f64() * 1000.0,
            "frames_per_kind": trace.len() / 2,
            "full_bytes": full_bytes,
            "diff_bytes": diff_bytes,
            "exported_history_bytes": history_bytes,
            "idle_rss_delta_kib": idle_rss.zip(rss_before).map(|(idle, before)| idle.saturating_sub(before)),
        }),
    );
    Ok(())
}

fn output(profile: &str, turn: u16) -> String {
    match profile {
        "codex" => format!(
            "\x1b[?25l\x1b[2K\x1b[1;38;2;120;165;225m• task {turn}\x1b[0m\r\n\
             \x1b[2K  \x1b[38;5;110m→ cargo check\x1b[0m 한글 e\u{301}\r\n\
             \x1b[2K  updated workspace files and checked the terminal adapter\r\n\
             \x1b[7m›\x1b[0m \x1b[?25h",
        ),
        "claude" => format!(
            "\x1b[?25l\x1b[2K\x1b[38;5;173m✻ operation {turn}\x1b[0m\r\n\
             \x1b[1;36m  Read\x1b[0m 4 files · src/terminal/session.rs\r\n\
             \x1b[2m  ⎿  … 32 lines · 画像 · e\u{301}\x1b[0m\r\n\
             \x1b[2K\x1b[?25h❯ ",
        ),
        _ => panic!("unknown benchmark workload {profile}"),
    }
}

fn current_rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_ascii_whitespace()
        .next()?
        .parse()
        .ok()
}
