use std::env;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
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
    let broker = args.first().is_some_and(|arg| arg == "app-server");
    let injected = args
        .iter()
        .any(|arg| arg == "--settings" || arg == "features.daemon_auto_start=false");
    if !broker && !injected {
        println!("NO-OBSERVERS-{provider}");
        return;
    }
    if provider == "codex"
        && !broker
        && let Some(path) = env::var_os("CORVID_FIXTURE_BROKER")
    {
        let status = Command::new(path)
            .args(["app-server", "--listen", "unix://", "--managed-daemon"])
            .status()
            .unwrap();
        std::process::exit(status.code().unwrap_or(1));
    }
    if let Some(launcher) = env::var_os("CORVID_FIXTURE_RESTART") {
        restart(provider, &args, &launcher);
        return;
    }

    observe(
        provider,
        r#""hook_event_name":"SessionStart","source":"startup""#,
    );
    if env::var_os("CORVID_FIXTURE_HANDOFF").is_some() {
        retained(provider);
        return;
    }
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
    // Replay the hook with a fresh delivery ID for the same turn.
    observe(provider, &stop);
    println!("DONE-{provider}");
}

fn restart(provider: &str, args: &[String], launcher: &std::ffi::OsStr) {
    let resumed = args.iter().any(|arg| arg == "--resume");
    if resumed {
        // Let the daemon sample the new executable before its first hook.
        println!("REEXECUTED");
        std::thread::sleep(std::time::Duration::from_secs(3));
        observe(
            provider,
            r#""hook_event_name":"SessionStart","source":"resume""#,
        );
        observe(
            provider,
            r#""hook_event_name":"UserPromptSubmit","prompt_id":"after-restart""#,
        );
        observe(
            provider,
            r#""hook_event_name":"Stop","stop_hook_active":false,"prompt_id":"after-restart""#,
        );
    } else {
        observe(
            provider,
            r#""hook_event_name":"SessionStart","source":"startup""#,
        );
        observe(
            provider,
            r#""hook_event_name":"UserPromptSubmit","prompt_id":"before-restart""#,
        );
        observe(
            provider,
            r#""hook_event_name":"PreToolUse","prompt_id":"before-restart","tool_use_id":"question","tool_name":"AskUserQuestion","tool_input":{}"#,
        );
    }
    println!("READY-{provider}");
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    if !resumed && input.trim() == "/restart" {
        observe(provider, r#""hook_event_name":"SessionEnd","reason":"other""#);
        let error = Command::new(launcher)
            .args(["--resume", "fixture-claude"])
            .args(args)
            .exec();
        panic!("fixture could not restart: {error}");
    }
}

fn retained(provider: &str) {
    let mut turn = 1;
    let fields = |turn| format!(r#""prompt_id":"turn-{turn}","turn_id":"turn-{turn}""#);
    observe(
        provider,
        &format!(r#""hook_event_name":"UserPromptSubmit",{}"#, fields(turn)),
    );
    println!("RETAINED-{provider}-PID={}", std::process::id());
    for line in io::stdin().lines() {
        let line = line.unwrap();
        let tool = if provider == "claude" {
            "AskUserQuestion"
        } else {
            "request_user_input"
        };
        let question = format!(
            r#""tool_name":"{tool}","tool_use_id":"question-{turn}",{}"#,
            fields(turn)
        );
        match line.trim() {
            "question" => observe(
                provider,
                &format!(r#""hook_event_name":"PreToolUse",{question}"#),
            ),
            "respond" => {
                observe(
                    provider,
                    &format!(r#""hook_event_name":"PostToolUse",{question}"#),
                );
                observe(
                    provider,
                    &format!(
                        r#""hook_event_name":"Stop","stop_hook_active":false,{}"#,
                        fields(turn)
                    ),
                );
                turn += 1;
                observe(
                    provider,
                    &format!(r#""hook_event_name":"UserPromptSubmit",{}"#, fields(turn)),
                );
            }
            "wrong-epoch" => {
                let original = env::var_os("CORVID_SERVER_EPOCH").unwrap();
                // Single-threaded fixture: use a foreign epoch for one submission.
                unsafe {
                    env::set_var(
                        "CORVID_SERVER_EPOCH",
                        "00000000-0000-4000-8000-000000000001",
                    )
                };
                observe(
                    provider,
                    &format!(r#""hook_event_name":"PreToolUse",{question}"#),
                );
                unsafe { env::set_var("CORVID_SERVER_EPOCH", original) };
            }
            "exit" => return,
            _ => panic!("unknown fixture command"),
        }
        println!("OBSERVED-{line}-PID={}", std::process::id());
    }
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
