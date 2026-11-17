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
    let (read, mut write) = io::pipe()?;
    writeln!(
        write,
        r#"{{"hook_event_name":"SessionStart","session_id":"{name}","source":"startup"}}"#
    )?;
    drop(write);
    let status = Command::new(std::env::var_os("CORVID_FIXTURE_BINARY").unwrap())
        .args(["observe", "--provider", "claude"])
        .stdin(read)
        .status()?;
    assert!(status.success());
    std::fs::write(root.join(format!("ready-{name}")), "")?;
    for line in io::stdin().lock().lines() {
        std::fs::write(root.join(format!("input-{name}")), line?)?;
    }
    Ok(())
}
