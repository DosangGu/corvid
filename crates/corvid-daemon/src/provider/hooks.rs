use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nix::fcntl::OFlag;
use semver::Version;
use serde_json::{Map, Value, json};

use corvid_core::agent::Provider;

pub(crate) struct Config {
    build: fn(&str) -> Map<String, Value>,
    format_settings: fn(Map<String, Value>) -> Value,
    arguments: fn(&str, Map<String, Value>) -> Vec<OsString>,
    installed_settings: Option<(&'static str, &'static str, &'static str)>,
    accepts_settings_override: bool,
    supports_session_injection: bool,
}

pub(super) const CLAUDE: Config = Config {
    build: claude_hooks,
    format_settings: |hooks| json!({ "hooks": hooks }),
    arguments: |_, hooks| {
        vec![
            "--settings".into(),
            json!({ "hooks": hooks }).to_string().into(),
        ]
    },
    installed_settings: Some(("CLAUDE_CONFIG_DIR", ".claude", "settings.json")),
    accepts_settings_override: true,
    supports_session_injection: true,
};

pub(super) const CODEX: Config = Config {
    build: codex_hooks,
    format_settings: |hooks| json!({ "hooks": hooks }),
    arguments: |executable, hooks| vec!["-c".into(), codex_override(executable, &hooks).into()],
    installed_settings: Some(("CODEX_HOME", ".codex", "hooks.json")),
    accepts_settings_override: false,
    supports_session_injection: true,
};

pub(super) const ANTIGRAVITY: Config = Config {
    build: antigravity_hooks,
    format_settings: |hooks| json!({ "corvid-observer": hooks }),
    arguments: |_, _| Vec::new(),
    installed_settings: None,
    accepts_settings_override: false,
    supports_session_injection: false,
};

const MAX_SETTINGS_BYTES: u64 = 256 * 1024;
const ANTIGRAVITY_EVENTS: [&str; 4] = ["SessionStart", "PreInvocation", "PostInvocation", "Stop"];
const CODEX_EVENTS: [(&str, Option<&str>); 9] = [
    ("SessionStart", Some("startup|resume|compact")),
    ("UserPromptSubmit", None),
    ("PreModelRequest", None),
    ("PostModelRequest", None),
    ("PreToolUse", Some("Bash|request_user_input")),
    ("PostToolUse", Some("Bash|request_user_input")),
    ("PermissionRequest", Some("Bash")),
    ("Stop", None),
    ("Interrupt", None),
];

pub fn observer_settings(provider: Provider) -> Result<Value> {
    let executable = observer_executable()?;
    let config = &super::contract(provider).unix.hooks;
    Ok((config.format_settings)(observer_hooks(
        provider,
        &executable,
    )))
}

pub(super) fn arguments(provider: Provider, version: &Version, argv: &[OsString]) -> Vec<OsString> {
    let config = &super::contract(provider).unix.hooks;
    if !config.supports_session_injection {
        return Vec::new();
    }
    if config.accepts_settings_override
        && argv
            .iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "--settings" || arg.to_string_lossy().starts_with("--settings="))
    {
        return Vec::new();
    }
    let Ok(executable) = observer_executable() else {
        return Vec::new();
    };
    let mut hooks = observer_hooks(provider, &executable);
    if provider == Provider::Codex && *version < Version::new(0, 165, 0) {
        // Keep the legacy hook config usable on releases without model request hooks.
        hooks.remove("PreModelRequest");
        hooks.remove("PostModelRequest");
    }
    if let Some(installed_location) = config.installed_settings
        && let Some(settings) = installed_settings(installed_location)
    {
        retain_missing_hooks(&mut hooks, &settings);
    }
    if hooks.is_empty() {
        return Vec::new();
    }
    (config.arguments)(&executable, hooks)
}

fn installed_settings(
    (variable, default_directory, filename): (&str, &str, &str),
) -> Option<Value> {
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
        let groups = groups.as_array_mut().expect("observer groups are arrays");
        if let Some(installed) = settings["hooks"][event].as_array() {
            groups.retain(|group| {
                !installed
                    .iter()
                    .any(|existing| groups_match(group, existing))
            });
        }
        !groups.is_empty()
    });
}

fn groups_match(expected: &Value, existing: &Value) -> bool {
    if expected == existing {
        return true;
    }
    if expected.get("matcher") != existing.get("matcher") {
        return false;
    }
    let (Some(expected_hooks), Some(existing_hooks)) = (
        expected.get("hooks").and_then(Value::as_array),
        existing.get("hooks").and_then(Value::as_array),
    ) else {
        return false;
    };
    !expected_hooks.is_empty()
        && expected_hooks.iter().all(|handler| {
            existing_hooks
                .iter()
                .any(|candidate| handlers_match(handler, candidate))
        })
}

fn handlers_match(expected: &Value, existing: &Value) -> bool {
    if expected == existing {
        return true;
    }
    if expected.get("type").and_then(Value::as_str) != Some("command")
        || existing.get("type").and_then(Value::as_str) != Some("command")
        || expected.get("timeout") != existing.get("timeout")
        || expected.get("onFailure") != existing.get("onFailure")
        || expected.get("args") != existing.get("args")
    {
        return false;
    }
    let (Some(expected_cmd), Some(existing_cmd)) = (
        expected.get("command").and_then(Value::as_str),
        existing.get("command").and_then(Value::as_str),
    ) else {
        return false;
    };
    if expected.get("args").is_some() {
        return same_canonical_executable(Path::new(expected_cmd), Path::new(existing_cmd));
    }
    let (Some((expected_path, expected_suffix)), Some((existing_path, existing_suffix))) = (
        parse_shell_observer_command(expected_cmd),
        parse_shell_observer_command(existing_cmd),
    ) else {
        return false;
    };
    expected_suffix == existing_suffix && same_canonical_executable(&expected_path, &existing_path)
}

fn parse_shell_observer_command(command: &str) -> Option<(PathBuf, &str)> {
    let mut rest = command.strip_prefix("exec ")?;
    if !rest.starts_with('\'') {
        return None;
    }
    let mut path = String::new();
    while let Some(after_open) = rest.strip_prefix('\'') {
        let close = after_open.find('\'')?;
        path.push_str(&after_open[..close]);
        rest = &after_open[close + 1..];
        if let Some(after_quote) = rest.strip_prefix("\"'\"") {
            path.push('\'');
            rest = after_quote;
        } else {
            break;
        }
    }
    if path.is_empty() || !rest.starts_with(" observe") {
        return None;
    }
    Some((PathBuf::from(path), rest))
}

fn same_canonical_executable(expected: &Path, existing: &Path) -> bool {
    let (Ok(expected), Ok(existing)) = (fs::canonicalize(expected), fs::canonicalize(existing))
    else {
        return false;
    };
    expected == existing
}

fn observer_hooks(provider: Provider, executable: &str) -> Map<String, Value> {
    (super::contract(provider).unix.hooks.build)(executable)
}

fn observer_executable() -> Result<String> {
    let executable =
        corvid_update::layout::observer_executable(&env::current_exe()?.canonicalize()?);
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
            json!([
                { "matcher": matcher, "hooks": [handler.clone()] },
                { "matcher": "^(Bash|Read)$", "hooks": [handler.clone()] },
            ]),
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

fn antigravity_hooks(executable: &str) -> Map<String, Value> {
    let path = executable.replace('\'', "'\"'\"'");
    let handler = json!({
        "type": "command",
        "command": format!("exec '{path}' observe --provider antigravity"),
        "timeout": 1,
    });
    let mut hooks = Map::new();
    for event in ANTIGRAVITY_EVENTS {
        hooks.insert(event.to_owned(), json!([handler.clone()]));
    }
    hooks
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

    #[test]
    fn matches_installed_observer_hooks_across_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("corvid-real");
        let symlink = temporary.path().join("corvid-link");
        let other = temporary.path().join("corvid-other");
        fs::write(&target, b"").unwrap();
        fs::write(&other, b"").unwrap();
        std::os::unix::fs::symlink(&target, &symlink).unwrap();

        let target_str = target.to_str().unwrap();
        let symlink_str = symlink.to_str().unwrap();
        let other_str = other.to_str().unwrap();
        let missing_str = temporary.path().join("missing-corvid");
        let missing_str = missing_str.to_str().unwrap();

        for provider in [Provider::Claude, Provider::Codex] {
            let mut hooks = observer_hooks(provider, target_str);
            let symlinked = observer_hooks(provider, symlink_str);
            let different = observer_hooks(provider, other_str);
            let missing = observer_hooks(provider, missing_str);
            let installed = json!({ "hooks": {
                "SessionStart": [symlinked["SessionStart"][0].clone()],
                "PermissionRequest": [different["PermissionRequest"][0].clone()],
                "Stop": [missing["Stop"][0].clone()],
            }});

            retain_missing_hooks(&mut hooks, &installed);

            assert!(!hooks.contains_key("SessionStart"));
            assert!(hooks.contains_key("PermissionRequest"));
            assert!(hooks.contains_key("Stop"));
        }
    }

    #[test]
    fn antigravity_hooks_use_flat_handler_arrays_without_pre_tool_use() {
        let formatted = (ANTIGRAVITY.format_settings)(antigravity_hooks("/opt/corvid"));
        let entry = formatted["corvid-observer"].as_object().unwrap();
        assert!(!entry.contains_key("PreToolUse"));
        assert!(!entry.contains_key("PostToolUse"));
        for event in ANTIGRAVITY_EVENTS {
            let handlers = entry[event].as_array().unwrap();
            assert_eq!(handlers.len(), 1);
            assert!(handlers[0].get("hooks").is_none());
            assert_eq!(
                handlers[0]["command"].as_str().unwrap(),
                "exec '/opt/corvid' observe --provider antigravity"
            );
            assert_eq!(handlers[0]["timeout"].as_u64(), Some(1));
        }
        assert!(arguments(Provider::Antigravity, &Version::new(1, 5, 17), &[]).is_empty());
    }
}
