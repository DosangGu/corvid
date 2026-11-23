// Synthetic native provider used only by the workspace restoration integration test.
use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

fn main() {
    let argv: Vec<_> = env::args().collect();
    let provider = Path::new(&argv[0]).file_stem().unwrap().to_str().unwrap();
    if argv[1..] == ["--version"] {
        println!(
            "{}",
            match provider {
                "claude" => format!("{} (Claude Code)", env!("CORVID_FIXTURE_CLAUDE")),
                "codex" => format!("codex-cli {}", env!("CORVID_FIXTURE_CODEX")),
                _ => panic!("unexpected provider"),
            }
        );
        return;
    }
    let option = |name: &str| {
        argv.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].clone())
    };
    let resumed = option("--resume").or_else(|| option("resume"));
    let mut session = resumed
        .clone()
        .or_else(|| option("--fixture-session"))
        .unwrap();
    if session == "missing" {
        std::process::exit(7);
    }
    if session == "mismatch" {
        session = "wrong-session".into();
    }
    if resumed.is_some() && session == "saved-claude" && Path::new("delay-claude-restore").is_file()
    {
        // Exceed the old TUI test deadline but confirm within the daemon's window.
        std::thread::sleep(std::time::Duration::from_secs(21));
    }
    let source = if resumed.is_some() {
        "resume"
    } else {
        "startup"
    };
    let started = Instant::now();
    observe(
        provider,
        &session,
        &format!(r#""hook_event_name":"SessionStart","source":"{source}""#),
    );
    let _ = std::fs::write(
        format!("observer-{session}-{source}.txt"),
        format!("observer exited successfully after {:?}", started.elapsed()),
    );
    if let Some(next) = option("--fixture-switch") {
        observe(
            provider,
            &session,
            r#""hook_event_name":"SessionEnd","reason":"resume","prompt_id":"switch-1""#,
        );
        observe(
            provider,
            &next,
            r#""hook_event_name":"SessionStart","source":"resume","prompt_id":"switch-1""#,
        );
        session = next;
    }
    if resumed.is_none() && provider == "claude" {
        observe(
            provider,
            &session,
            r#""hook_event_name":"UserPromptSubmit","prompt_id":"turn-1""#,
        );
        observe(
            provider,
            &session,
            r#""hook_event_name":"PreToolUse","prompt_id":"turn-1","tool_use_id":"question-1","tool_name":"AskUserQuestion","tool_input":{}"#,
        );
    }
    println!("READY-{session}");
    for line in io::stdin().lock().lines() {
        if line.unwrap().trim() == "exit" {
            break;
        }
    }
}

fn observe(provider: &str, session: &str, fields: &str) {
    // Every fixture provider runs in the same private directory. Keep their
    // short-lived observers from competing for the production hook deadline.
    let observer_slot = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open("fixture-observer.lock")
        .unwrap();
    observer_slot.lock().unwrap();

    // Have the synthetic payload and EOF ready when the observer's input deadline starts.
    let (input, mut writer) = io::pipe().unwrap();
    writeln!(writer, r#"{{"session_id":"{session}",{fields}}}"#).unwrap();
    drop(writer);

    let status = Command::new(env::var_os("CORVID_FIXTURE_BINARY").unwrap())
        .args(["observe", "--provider", provider])
        .stdin(input)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}
