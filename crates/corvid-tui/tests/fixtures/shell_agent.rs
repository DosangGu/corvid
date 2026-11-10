use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

fn main() {
    let mut args = env::args();
    let invoked = args.next().unwrap();
    let provider = Path::new(&invoked).file_name().unwrap().to_str().unwrap();
    let args: Vec<_> = args.collect();
    if args == ["--version"] {
        let version = match provider {
            "claude" => format!("{} (Claude Code)", env!("CORVID_FIXTURE_CLAUDE")),
            "codex" => format!("codex-cli {}", env!("CORVID_FIXTURE_CODEX")),
            _ => panic!("unexpected provider"),
        };
        println!("{version}");
        return;
    }
    let injected = args
        .iter()
        .any(|arg| arg == "--settings" || arg == "features.daemon_auto_start=false");
    if !injected {
        println!("NO-OBSERVERS-{provider}");
        return;
    }

    observe(
        provider,
        r#""hook_event_name":"SessionStart","source":"startup""#,
    );
    println!("READY-{provider}");
    let mut line = String::new();
    io::stdin().read_line(&mut line).unwrap();
    let turn = r#""prompt_id":"turn-1","turn_id":"turn-1""#;
    observe(
        provider,
        &format!(r#""hook_event_name":"UserPromptSubmit",{turn}"#),
    );
    let stop = format!(r#""hook_event_name":"Stop","stop_hook_active":false,{turn}"#);
    observe(provider, &stop);
    observe(provider, &stop);
    println!("DONE-{provider}");
}

fn observe(provider: &str, fields: &str) {
    let mut child = Command::new(env::var_os("CORVID_FIXTURE_BINARY").unwrap())
        .args(["observe", "--provider", provider])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        r#"{{"session_id":"fixture-{provider}",{fields}}}"#
    )
    .unwrap();
    assert!(child.wait().unwrap().success());
}
