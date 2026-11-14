// Synthetic native provider used only by the workspace restoration integration test.
use std::env;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};

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
    let mut child = Command::new(env::var_os("CORVID_FIXTURE_BINARY").unwrap())
        .args(["observe", "--provider", provider])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        r#"{{"session_id":"{session}",{fields}}}"#
    )
    .unwrap();
    assert!(child.wait().unwrap().success());
}
