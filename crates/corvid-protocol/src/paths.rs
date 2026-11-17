//! State and local endpoint locations shared by Corvid entry points.

#[cfg(any(test, target_os = "macos"))]
use std::io;
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
use std::{ffi::OsStr, path::Path};
use std::{ffi::OsString, path::PathBuf};

use anyhow::{Context, Result};
use uuid::Uuid;

#[cfg(test)]
mod tests;

const VIEW_STATE_DIR: &str = "CORVID_VIEW_STATE_DIR";
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
const COMPANION_SOCKET_NAME: &str = "corvid-companion";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateRole {
    Daemon,
    Companion,
    /// Uses the companion store and retains Deck's Linux HOME fallback.
    AgentDeck,
    TuiViews,
}

/// A resolved location. Preparing it may create the private macOS boot parent.
#[derive(Debug)]
pub struct RuntimePath {
    path: PathBuf,
    #[cfg(any(test, target_os = "macos"))]
    boot_parent: Option<PathBuf>,
}

impl RuntimePath {
    fn direct(path: PathBuf) -> Self {
        Self {
            path,
            #[cfg(any(test, target_os = "macos"))]
            boot_parent: None,
        }
    }

    /// Prepare the private boot parent, when applicable. The endpoint directory
    /// itself is opened or created separately by the caller's transport.
    pub fn prepare(self) -> Result<PathBuf> {
        #[cfg(target_os = "macos")]
        if let Some(parent) = self.boot_parent {
            crate::unix::RuntimeDir::create_all(&parent)?;
        }
        Ok(self.path)
    }

    #[cfg(any(test, target_os = "linux", target_os = "macos"))]
    fn join(mut self, name: impl AsRef<OsStr>) -> Self {
        self.path.push(name.as_ref());
        self
    }
}

trait PathInputs {
    fn var(&self, name: &str) -> Option<OsString>;

    fn nonempty(&self, name: &str) -> Option<PathBuf> {
        self.var(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    #[cfg(any(test, target_os = "macos"))]
    fn boot_session(&self) -> io::Result<String>;

    #[cfg(any(test, target_os = "linux"))]
    fn user_runtime(&self) -> Option<PathBuf>;
}

struct System;

impl PathInputs for System {
    fn var(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }

    #[cfg(target_os = "macos")]
    fn boot_session(&self) -> io::Result<String> {
        crate::macos::boot_session()
    }

    #[cfg(all(test, not(target_os = "macos")))]
    fn boot_session(&self) -> io::Result<String> {
        unreachable!("only macOS queries the boot session")
    }

    #[cfg(target_os = "linux")]
    fn user_runtime(&self) -> Option<PathBuf> {
        let uid = nix::unistd::geteuid().as_raw();
        let path = PathBuf::from(format!("/run/user/{uid}"));
        path.is_dir().then_some(path)
    }

    #[cfg(all(test, not(target_os = "linux")))]
    fn user_runtime(&self) -> Option<PathBuf> {
        unreachable!("only Linux checks the systemd user runtime")
    }
}

trait PlatformPaths {
    const DAEMON_SUBDIR: Option<&'static str> = Some("daemon");

    fn state_root(inputs: &impl PathInputs, role: StateRole) -> Result<PathBuf>;
    fn default_runtime(inputs: &impl PathInputs) -> Result<RuntimePath>;

    fn state(
        inputs: &impl PathInputs,
        role: StateRole,
        explicit: Option<PathBuf>,
    ) -> Result<PathBuf> {
        if let Some(path) = explicit {
            return Ok(path);
        }
        if role == StateRole::TuiViews
            && let Some(path) = inputs.var(VIEW_STATE_DIR)
        {
            return Ok(PathBuf::from(path).join("tui"));
        }

        let root = Self::state_root(inputs, role)?;
        Ok(match role {
            StateRole::Daemon => match Self::DAEMON_SUBDIR {
                Some(name) => root.join(name),
                None => root,
            },
            StateRole::Companion | StateRole::AgentDeck => root.join("companion"),
            StateRole::TuiViews => root.join("clients").join("tui"),
        })
    }

    fn runtime(
        inputs: &impl PathInputs,
        explicit: Option<PathBuf>,
        previous: impl FnOnce() -> Option<PathBuf>,
    ) -> Result<RuntimePath> {
        if let Some(path) = explicit
            .or_else(|| inputs.var(crate::collector::RUNTIME_DIR).map(PathBuf::from))
            .or_else(previous)
        {
            return Ok(RuntimePath::direct(path));
        }
        Self::default_runtime(inputs)
    }
}

#[cfg(any(test, target_os = "linux"))]
struct Linux;
#[cfg(any(test, target_os = "macos"))]
struct MacOs;
#[cfg(any(test, windows))]
struct Windows;

#[cfg(target_os = "linux")]
type Native = Linux;
#[cfg(target_os = "macos")]
type Native = MacOs;
#[cfg(windows)]
type Native = Windows;

#[cfg(any(test, target_os = "linux"))]
impl PlatformPaths for Linux {
    const DAEMON_SUBDIR: Option<&'static str> = None;

    fn state_root(inputs: &impl PathInputs, role: StateRole) -> Result<PathBuf> {
        if let Some(base) = inputs.nonempty("XDG_STATE_HOME") {
            return Ok(base.join("corvid"));
        }

        let home = match role {
            // The standalone Deck historically accepted an empty HOME here.
            StateRole::AgentDeck => inputs.var("HOME").map(PathBuf::from),
            _ => inputs.nonempty("HOME"),
        };
        let hint = match role {
            StateRole::Companion => {
                "set HOME, XDG_STATE_HOME, or choose a companion state directory"
            }
            StateRole::AgentDeck => "set XDG_STATE_HOME or pass --state-dir",
            _ => "set XDG_STATE_HOME or --state-dir",
        };
        Ok(home.context(hint)?.join(".local/state/corvid"))
    }

    fn default_runtime(inputs: &impl PathInputs) -> Result<RuntimePath> {
        let base = inputs
            .nonempty("XDG_RUNTIME_DIR")
            .context("set XDG_RUNTIME_DIR or --runtime-dir")?;
        Ok(RuntimePath::direct(base.join("corvid")))
    }
}

#[cfg(any(test, target_os = "macos"))]
impl PlatformPaths for MacOs {
    fn state_root(inputs: &impl PathInputs, _: StateRole) -> Result<PathBuf> {
        Ok(macos_home(inputs)?.join("Library/Application Support/Corvid"))
    }

    fn default_runtime(inputs: &impl PathInputs) -> Result<RuntimePath> {
        let parent = macos_home(inputs)?.join(".corvid");
        let boot = inputs.boot_session()?;
        anyhow::ensure!(
            boot.len() == 36 && is_boot_id(&boot),
            "invalid boot session"
        );
        Ok(RuntimePath {
            path: parent.join(format!("r-{}", &boot[..16])),
            boot_parent: Some(parent),
        })
    }
}

#[cfg(any(test, target_os = "macos"))]
fn macos_home(inputs: &impl PathInputs) -> io::Result<PathBuf> {
    inputs
        .nonempty("HOME")
        .ok_or_else(|| io::Error::other("HOME is unavailable"))
}

#[cfg(any(test, target_os = "macos"))]
fn is_boot_id(id: &str) -> bool {
    id.bytes()
        .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

#[cfg(any(test, windows))]
impl PlatformPaths for Windows {
    fn state_root(inputs: &impl PathInputs, role: StateRole) -> Result<PathBuf> {
        let hint = match role {
            StateRole::Companion | StateRole::AgentDeck => {
                "set LOCALAPPDATA or choose a companion state directory"
            }
            _ => "LOCALAPPDATA is unavailable",
        };
        Ok(inputs
            .nonempty("LOCALAPPDATA")
            .context(hint)?
            .join("Corvid"))
    }

    fn default_runtime(inputs: &impl PathInputs) -> Result<RuntimePath> {
        Ok(RuntimePath::direct(
            Self::state_root(inputs, StateRole::Daemon)?.join("runtime"),
        ))
    }
}

pub fn state_directory(role: StateRole, explicit: Option<PathBuf>) -> Result<PathBuf> {
    Native::state(&System, role, explicit)
}

/// Explicit selection precedes the environment, then the saved navigation hint
/// (if supplied) and the platform default.
pub fn daemon_runtime(
    explicit: Option<PathBuf>,
    previous: impl FnOnce() -> Option<PathBuf>,
) -> Result<RuntimePath> {
    Native::runtime(&System, explicit, previous)
}

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy)]
enum UnixEndpoint {
    Companion,
    Monitor(Option<Uuid>),
    AgentDeck(Uuid),
}

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
trait UnixPaths: PlatformPaths {
    const SLOT_PREFIX: &'static str;
    const DECK_PREFIX: &'static str;

    fn endpoint_base(
        inputs: &impl PathInputs,
        role: UnixEndpoint,
        local_runtime: Option<&Path>,
    ) -> Result<RuntimePath>;

    fn endpoint(
        inputs: &impl PathInputs,
        role: UnixEndpoint,
        local_runtime: Option<&Path>,
    ) -> Result<RuntimePath> {
        let name = match role {
            UnixEndpoint::Companion | UnixEndpoint::Monitor(None) => {
                COMPANION_SOCKET_NAME.to_owned()
            }
            UnixEndpoint::Monitor(Some(controller)) => {
                format!("{}{}", Self::SLOT_PREFIX, controller.simple())
            }
            UnixEndpoint::AgentDeck(controller) => {
                format!("{}{}", Self::DECK_PREFIX, controller.simple())
            }
        };
        Ok(Self::endpoint_base(inputs, role, local_runtime)?.join(name))
    }
}

#[cfg(any(test, target_os = "linux"))]
impl UnixPaths for Linux {
    const SLOT_PREFIX: &'static str = "cv-slot-";
    const DECK_PREFIX: &'static str = "cv-deck-";

    fn endpoint_base(
        inputs: &impl PathInputs,
        role: UnixEndpoint,
        local_runtime: Option<&Path>,
    ) -> Result<RuntimePath> {
        // Presentation endpoints use XDG_RUNTIME_DIR directly, not its corvid child.
        if let Some(base) = inputs.nonempty("XDG_RUNTIME_DIR") {
            return Ok(RuntimePath::direct(base));
        }

        let hint = match role {
            UnixEndpoint::Companion => "set XDG_RUNTIME_DIR or companion --socket-dir",
            UnixEndpoint::AgentDeck(_) => "set XDG_RUNTIME_DIR for Agent Deck",
            UnixEndpoint::Monitor(_) => {
                // An SSH session may lack XDG_RUNTIME_DIR even with a running user manager.
                return inputs
                    .user_runtime()
                    .or_else(|| local_runtime.map(Path::to_owned))
                    .map(RuntimePath::direct)
                    .or_else(|| Self::runtime(inputs, None, || None).ok())
                    .context("set XDG_RUNTIME_DIR for the agent monitor");
            }
        };
        anyhow::bail!(hint)
    }
}

#[cfg(any(test, target_os = "macos"))]
impl UnixPaths for MacOs {
    const SLOT_PREFIX: &'static str = "s-";
    const DECK_PREFIX: &'static str = "d-";

    fn endpoint_base(
        inputs: &impl PathInputs,
        _: UnixEndpoint,
        _: Option<&Path>,
    ) -> Result<RuntimePath> {
        Self::runtime(inputs, None, || None)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn companion_socket(explicit: Option<PathBuf>) -> Result<RuntimePath> {
    match explicit {
        Some(path) => Ok(RuntimePath::direct(path)),
        None => Native::endpoint(&System, UnixEndpoint::Companion, None),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn monitor_socket(
    controller: Option<Uuid>,
    local_runtime: Option<&Path>,
) -> Result<RuntimePath> {
    Native::endpoint(&System, UnixEndpoint::Monitor(controller), local_runtime)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn agent_deck_socket(controller: Uuid) -> Result<RuntimePath> {
    Native::endpoint(&System, UnixEndpoint::AgentDeck(controller), None)
}

/// The macOS Deck prototype always uses the boot-scoped companion default.
#[cfg(target_os = "macos")]
pub fn macos_default_companion_socket() -> Result<RuntimePath> {
    Ok(MacOs::default_runtime(&System)?.join(COMPANION_SOCKET_NAME))
}

#[cfg(any(test, target_os = "macos"))]
fn named_socket(inputs: &impl PathInputs, name: &OsStr) -> Result<RuntimePath> {
    anyhow::ensure!(
        Path::new(name).file_name() == Some(name),
        "invalid socket name"
    );
    Ok(MacOs::runtime(inputs, None, || None)?.join(name))
}

#[cfg(target_os = "macos")]
pub fn macos_named_socket(name: &OsStr) -> Result<RuntimePath> {
    named_socket(&System, name)
}

#[cfg(any(test, target_os = "macos"))]
fn startup_socket_name<'a>(
    inputs: &impl PathInputs,
    address: &'a Path,
) -> Result<Option<&'a OsStr>> {
    // A monitor address can come from saved discovery metadata for a prior boot.
    // Persist the socket name so login startup resolves it in the new session.
    let parent = address.parent();
    let prior_boot = macos_home(inputs).is_ok_and(|home| {
        parent
            .and_then(|path| path.strip_prefix(home.join(".corvid")).ok())
            .and_then(Path::to_str)
            .and_then(|name| name.strip_prefix("r-"))
            .is_some_and(|id| id.len() == 16 && is_boot_id(id))
    });
    if prior_boot || parent == Some(MacOs::runtime(inputs, None, || None)?.path.as_path()) {
        return Ok(Some(address.file_name().context("missing socket name")?));
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
pub fn macos_startup_socket_name(address: &Path) -> Result<Option<&OsStr>> {
    startup_socket_name(&System, address)
}

#[cfg(windows)]
pub const AGENT_DECK_PIPE_PREFIX: &str = "corvid-agent-deck-v1";

#[cfg(any(test, windows))]
pub fn monitor_pipe_instance(shared_instance: &str, controller: Option<Uuid>) -> String {
    controller.map_or_else(|| shared_instance.to_owned(), controller_pipe_instance)
}

#[cfg(any(test, windows))]
pub fn controller_pipe_instance(controller: Uuid) -> String {
    controller.simple().to_string()
}
