// Synthetic native provider used only by the workspace restoration integration test.
use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::Command;

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
    observe(
        provider,
        &session,
        &format!(
            r#""hook_event_name":"SessionStart","source":"{}""#,
            if resumed.is_some() {
                "resume"
            } else {
                "startup"
            }
        ),
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

    let mut observer = Command::new(env::var_os("CORVID_FIXTURE_OBSERVER").unwrap());
    #[cfg(windows)]
    observer.args(["observe", "--provider", provider]);
    #[cfg(unix)]
    observer
        .args(["--ignored", "--exact", "fixture_observer::collect"])
        .env("CORVID_FIXTURE_PROVIDER", provider);
    let result = observer.stdin(input).output().unwrap();
    if !result.status.success() {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open("fixture-observer.log")
            .unwrap();
        writeln!(
            log,
            "{provider} {session} {fields}: {}\n{}\n{}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr),
        )
        .unwrap();
        panic!("fixture observer failed for {provider} {session}");
    }
}
