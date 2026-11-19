use anyhow::{Context, Result};
use corvid_core::agent::Provider;
use serde_json::{Map, Value, json};

pub(crate) struct Config {
    events: &'static [(&'static str, Option<&'static str>)],
    handler: fn(&str, &str) -> Value,
}

pub(super) const CLAUDE: Config = Config {
    events: &[
        ("SessionStart", None),
        ("UserPromptSubmit", None),
        ("PreToolUse", Some("^(AskUserQuestion|ExitPlanMode)$")),
        ("PostToolUse", Some("^(AskUserQuestion|ExitPlanMode)$")),
        ("PermissionRequest", None),
        ("Stop", None),
        ("SessionEnd", None),
    ],
    handler: |executable, _| {
        json!({
            "type": "command", "command": executable,
            "args": ["observe", "--provider", "claude"], "timeout": 1,
            "onFailure": "continue"
        })
    },
};

pub(super) const CODEX: Config = Config {
    events: &[
        ("SessionStart", Some("startup|resume|compact")),
        ("UserPromptSubmit", None),
        ("PreToolUse", Some("Bash|request_user_input")),
        ("PostToolUse", Some("Bash|request_user_input")),
        ("PermissionRequest", Some("Bash")),
        ("Stop", None),
        ("Interrupt", None),
    ],
    handler: codex_handler,
};

fn codex_handler(executable: &str, event: &str) -> Value {
    let path = executable.replace('\'', "''");
    let neutral = if event == "Stop" {
        " --neutral-json"
    } else {
        ""
    };
    json!({
        "type": "command",
        "command": format!("& '{path}' observe --provider codex{neutral}"),
        "timeout": 1
    })
}

pub fn observer_settings(provider: Provider) -> Result<Value> {
    let executable = corvid_update::layout::observer_executable(&std::env::current_exe()?);
    let executable = executable
        .to_str()
        .context("hook executable path must be UTF-8")?;
    let config = &super::contract(provider).windows.hooks;
    let mut hooks = Map::new();
    for (event, matcher) in config.events {
        let handler = (config.handler)(executable, event);
        let mut group = json!({ "hooks": [handler] });
        if let Some(matcher) = matcher {
            group["matcher"] = json!(matcher);
        }
        hooks.insert((*event).into(), json!([group]));
    }
    Ok(json!({ "hooks": hooks }))
}
