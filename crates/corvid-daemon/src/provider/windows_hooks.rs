use anyhow::{Context, Result, bail};
use corvid_core::agent::Provider;
use serde_json::{Map, Value, json};

pub(crate) struct Config {
    events: &'static [(&'static str, Option<&'static str>)],
    handler: fn(&str, &str) -> Result<Value>,
    top_key: &'static str,
    grouped: bool,
}

pub(super) const CLAUDE: Config = Config {
    events: &[
        ("SessionStart", None),
        ("UserPromptSubmit", None),
        ("PreToolUse", Some("^(AskUserQuestion|ExitPlanMode)$")),
        ("PostToolUse", Some("^(AskUserQuestion|ExitPlanMode)$")),
        ("PreToolUse", Some("^(Bash|Read)$")),
        ("PostToolUse", Some("^(Bash|Read)$")),
        ("PermissionRequest", None),
        ("Stop", None),
        ("SessionEnd", None),
    ],
    handler: |executable, _| {
        Ok(json!({
            "type": "command", "command": executable,
            "args": ["observe", "--provider", "claude"], "timeout": 1,
            "onFailure": "continue"
        }))
    },
    top_key: "hooks",
    grouped: true,
};

pub(super) const CODEX: Config = Config {
    events: &[
        ("SessionStart", Some("startup|resume|compact")),
        ("UserPromptSubmit", None),
        ("PreModelRequest", None),
        ("PostModelRequest", None),
        ("PreToolUse", Some("Bash|request_user_input")),
        ("PostToolUse", Some("Bash|request_user_input")),
        ("PermissionRequest", Some("Bash")),
        ("Stop", None),
        ("Interrupt", None),
    ],
    handler: codex_handler,
    top_key: "hooks",
    grouped: true,
};

pub(super) const ANTIGRAVITY: Config = Config {
    events: &[
        ("SessionStart", None),
        ("PreInvocation", None),
        ("PostInvocation", None),
        ("Stop", None),
    ],
    handler: antigravity_handler,
    top_key: "corvid-observer",
    grouped: false,
};

fn codex_handler(executable: &str, event: &str) -> Result<Value> {
    let path = executable.replace('\'', "''");
    let neutral = if event == "Stop" {
        " --neutral-json"
    } else {
        ""
    };
    Ok(json!({
        "type": "command",
        "command": format!("& '{path}' observe --provider codex{neutral}"),
        "timeout": 1
    }))
}

fn antigravity_handler(executable: &str, _: &str) -> Result<Value> {
    if executable
        .chars()
        .any(|ch| ch.is_control() || "\"%&|^<>!".contains(ch))
    {
        bail!("hook executable path contains characters unsupported by cmd.exe");
    }
    Ok(json!({
        "type": "command",
        "command": format!("\"{executable}\" observe --provider antigravity"),
        "timeout": 1
    }))
}

pub fn observer_settings(provider: Provider) -> Result<Value> {
    let executable = corvid_update::layout::observer_executable(&std::env::current_exe()?);
    let executable = executable
        .to_str()
        .context("hook executable path must be UTF-8")?;
    let config = &super::contract(provider).windows.hooks;
    let mut hooks = Map::new();
    for (event, matcher) in config.events {
        let handler = (config.handler)(executable, event)?;
        let entry = if config.grouped {
            let mut group = json!({ "hooks": [handler] });
            if let Some(matcher) = matcher {
                group["matcher"] = json!(matcher);
            }
            group
        } else {
            handler
        };
        hooks
            .entry((*event).to_owned())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("observer groups are arrays")
            .push(entry);
    }
    Ok(json!({ config.top_key: hooks }))
}
