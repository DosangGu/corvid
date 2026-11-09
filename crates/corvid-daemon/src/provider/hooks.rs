use std::env;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use nix::fcntl::OFlag;
use portable_pty::CommandBuilder;
use serde_json::{Map, Value, json};

use corvid_core::agent::Provider;

const MAX_SETTINGS_BYTES: u64 = 256 * 1024;
const CODEX_EVENTS: [(&str, Option<&str>); 7] = [
    ("SessionStart", Some("startup|resume|compact")),
    ("UserPromptSubmit", None),
    ("PreToolUse", Some("Bash|request_user_input")),
    ("PostToolUse", Some("Bash|request_user_input")),
    ("PermissionRequest", Some("Bash")),
    ("Stop", None),
    ("Interrupt", None),
];

pub fn observer_settings(provider: Provider) -> Result<Value> {
    let executable = observer_executable()?;
    Ok(json!({ "hooks": observer_hooks(provider, &executable) }))
}

pub(crate) fn configure(command: &mut CommandBuilder, provider: Provider, argv: &[OsString]) {
    if provider == Provider::Claude
        && argv
            .iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "--settings" || arg.to_string_lossy().starts_with("--settings="))
    {
        return;
    }
    let Ok(executable) = observer_executable() else {
        return;
    };
    let mut hooks = observer_hooks(provider, &executable);
    if let Some(settings) = installed_settings(provider) {
        retain_missing_hooks(&mut hooks, &settings);
    }
    if hooks.is_empty() {
        return;
    }
    match provider {
        Provider::Claude => {
            command.arg("--settings");
            command.arg(json!({ "hooks": hooks }).to_string());
        }
        Provider::Codex => {
            command.arg("-c");
            command.arg(codex_override(&executable, &hooks));
        }
    }
}

fn installed_settings(provider: Provider) -> Option<Value> {
    let (variable, default_directory, filename) = match provider {
        Provider::Claude => ("CLAUDE_CONFIG_DIR", ".claude", "settings.json"),
        Provider::Codex => ("CODEX_HOME", ".codex", "hooks.json"),
    };
    let directory = env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(default_directory)))?;
    if !directory.is_absolute() {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OFlag::O_NONBLOCK.bits())
        .open(directory.join(filename))
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SETTINGS_BYTES {
        return None;
    }
    serde_json::from_reader(file.take(MAX_SETTINGS_BYTES + 1)).ok()
}

fn retain_missing_hooks(hooks: &mut Map<String, Value>, settings: &Value) {
    hooks.retain(|event, groups| {
        settings["hooks"][event]
            .as_array()
            .is_none_or(|installed| !installed.contains(&groups[0]))
    });
}

fn observer_hooks(provider: Provider, executable: &str) -> Map<String, Value> {
    match provider {
        Provider::Claude => claude_hooks(executable),
        Provider::Codex => codex_hooks(executable),
    }
}

fn observer_executable() -> Result<String> {
    let executable = env::current_exe()?.canonicalize()?;
    let executable = executable
        .to_str()
        .context("hook executable path must be UTF-8")?;
    Ok(executable.to_owned())
}

fn claude_hooks(executable: &str) -> Map<String, Value> {
    let handler = json!({
        "type": "command",
        "command": executable,
        "args": ["observe", "--provider", "claude"],
        "timeout": 1,
        "onFailure": "continue",
    });
    let mut hooks = Map::new();
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PermissionRequest",
        "Stop",
        "SessionEnd",
        "SubagentStart",
        "SubagentStop",
    ] {
        hooks.insert(event.to_owned(), json!([{ "hooks": [handler.clone()] }]));
    }
    for (event, matcher) in [
        ("PreToolUse", "^(AskUserQuestion|ExitPlanMode)$"),
        ("PostToolUse", "^(AskUserQuestion|ExitPlanMode|Agent)$"),
    ] {
        hooks.insert(
            event.to_owned(),
            json!([{ "matcher": matcher, "hooks": [handler.clone()] }]),
        );
    }
    let mut batch = handler;
    batch["args"] = json!(["observe", "--provider", "claude", "--claude-input-batch"]);
    hooks.insert("PostToolBatch".to_owned(), json!([{ "hooks": [batch] }]));
    hooks
}

fn codex_hooks(executable: &str) -> Map<String, Value> {
    let mut hooks = Map::new();
    for (event, matcher) in CODEX_EVENTS {
        let command = codex_command(executable, event);
        let mut group =
            json!({ "hooks": [{ "type": "command", "command": command, "timeout": 1 }] });
        if let Some(matcher) = matcher {
            group["matcher"] = json!(matcher);
        }
        hooks.insert(event.to_owned(), json!([group]));
    }
    hooks
}

fn codex_override(executable: &str, hooks: &Map<String, Value>) -> String {
    let mut groups = Vec::with_capacity(hooks.len());
    for (event, matcher) in CODEX_EVENTS {
        if !hooks.contains_key(event) {
            continue;
        }
        // TOML basic strings also require escaping DEL.
        let command = json!(codex_command(executable, event))
            .to_string()
            .replace('\u{7f}', "\\u007f");
        let matcher = matcher
            .map(|value| format!(", matcher = {}", json!(value)))
            .unwrap_or_default();
        groups.push(format!(
            "{event} = [{{ hooks = [{{ type = \"command\", command = {command}, timeout = 1 }}]{matcher} }}]"
        ));
    }
    format!("hooks = {{ {} }}", groups.join(", "))
}

fn codex_command(executable: &str, event: &str) -> String {
    let path = executable.replace('\'', "'\"'\"'");
    let mut command = format!("exec '{path}' observe --provider codex");
    if event == "Stop" {
        command.push_str(" --neutral-json");
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_only_missing_groups_when_an_observer_is_already_installed() {
        for provider in [Provider::Claude, Provider::Codex] {
            let mut hooks = observer_hooks(provider, "/opt/corvid");
            let installed = json!({ "hooks": {
                "SessionStart": [hooks["SessionStart"][0].clone(), { "hooks": [] }],
                "Stop": [{ "hooks": [{ "type": "command", "command": "/opt/custom" }] }],
            }});

            retain_missing_hooks(&mut hooks, &installed);

            assert!(!hooks.contains_key("SessionStart"));
            assert!(hooks.contains_key("UserPromptSubmit"));
            assert!(hooks.contains_key("Stop"));
        }
    }
}
