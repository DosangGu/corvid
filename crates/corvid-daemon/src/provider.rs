mod contracts;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod hooks;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod profile;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod shell;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(windows)]
pub(crate) mod windows_hooks;

use corvid_core::agent::AgentSessionKey;
#[cfg(windows)]
use corvid_core::agent::Provider;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use contracts::InProcessResume;
pub(crate) use contracts::{CONTRACTS, contract, from_wire};
pub(crate) use contracts::{Resume, ResumeConfirmation};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use profile::adapter;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::*;

pub(crate) fn valid_session(key: &AgentSessionKey) -> bool {
    contracts::registered(key.provider).is_some_and(|integration| {
        key.has_valid_id() && (integration.valid_session_id)(&key.provider_session_id)
    })
}

#[cfg(windows)]
pub(crate) fn windows_provider(name: &str) -> Option<Provider> {
    CONTRACTS
        .iter()
        .find(|integration| name.eq_ignore_ascii_case(integration.windows.program))
        .map(|integration| integration.provider)
}
