use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::Command;

fn main() -> io::Result<()> {
    let name = std::env::args().last().unwrap();
    if name == "--version" {
        println!("2.1.318 (Claude Code)");
        return Ok(());
    }
    assert!(matches!(name.as_str(), "first" | "second"));
    let root = PathBuf::from(std::env::var_os("CORVID_FIXTURE_DIR").unwrap());
    observe("SessionStart", &name, r#""source":"startup""#)?;
    if name == "first" {
        observe("UserPromptSubmit", &name, r#""prompt_id":"turn""#)?;
    }
    std::fs::write(root.join(format!("ready-{name}")), "")?;
    for line in io::stdin().lock().lines() {
        let line = line?;
        std::fs::write(root.join(format!("input-{name}")), &line)?;
        if line == format!("deck-focus-{name}") {
            let event = if name == "first" {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while !root.join("finish-turn").exists() {
                    assert!(std::time::Instant::now() < deadline, "waiting for tooltip");
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                "Stop"
            } else {
                "UserPromptSubmit"
            };
            observe(event, &name, r#""prompt_id":"turn""#)?;
        }
    }
    Ok(())
}

fn observe(event: &str, session: &str, fields: &str) -> io::Result<()> {
    let (read, mut write) = io::pipe()?;
    writeln!(
        write,
        r#"{{"hook_event_name":"{event}","session_id":"{session}",{fields}}}"#
    )?;
    drop(write);
    let status = Command::new(std::env::var_os("CORVID_FIXTURE_BINARY").unwrap())
        .args(["observe", "--provider", "claude"])
        .stdin(read)
        .status()?;
    assert!(status.success());
    Ok(())
}
