use anyhow::{Context, Result};
use corvid_core::agent::Provider;
use serde_json::{Map, Value, json};

pub fn observer_settings(provider: Provider) -> Result<Value> {
    let executable = std::env::current_exe()?;
    let executable = executable
        .to_str()
        .context("hook executable path must be UTF-8")?;
    let mut hooks = Map::new();
    let events: &[(&str, Option<&str>)] = match provider {
        Provider::Claude => &[
            ("SessionStart", None),
            ("UserPromptSubmit", None),
            ("PreToolUse", Some("^(AskUserQuestion|ExitPlanMode)$")),
            ("PostToolUse", Some("^(AskUserQuestion|ExitPlanMode)$")),
            ("PermissionRequest", None),
            ("Stop", None),
            ("SessionEnd", None),
        ],
        Provider::Codex => &[
            ("SessionStart", Some("startup|resume|compact")),
            ("UserPromptSubmit", None),
            ("PreToolUse", Some("Bash|request_user_input")),
            ("PostToolUse", Some("Bash|request_user_input")),
            ("PermissionRequest", Some("Bash")),
            ("Stop", None),
            ("Interrupt", None),
        ],
    };
    for (event, matcher) in events {
        let handler = match provider {
            Provider::Claude => json!({
                "type": "command", "command": executable,
                "args": ["observe", "--provider", "claude"], "timeout": 1,
                "onFailure": "continue"
            }),
            Provider::Codex => {
                let path = executable.replace('\'', "''");
                let neutral = if *event == "Stop" {
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
        };
        let mut group = json!({ "hooks": [handler] });
        if let Some(matcher) = matcher {
            group["matcher"] = json!(matcher);
        }
        hooks.insert((*event).into(), json!([group]));
    }
    Ok(json!({ "hooks": hooks }))
}
