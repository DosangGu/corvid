#[cfg(windows)]
mod doctor;
mod executor;
mod ipc;
mod last_known;
#[cfg(any(windows, target_os = "macos"))]
pub mod launch;
pub mod monitor;
mod platform;
pub mod priority;
mod remote;
mod service;
mod slots;
mod store;

#[cfg(windows)]
pub use doctor::{Check, CheckStatus, diagnose};
pub use ipc::run_native;
pub use ipc::{
    PRESENTATION_ATTENTION_CAPABILITY, PRESENTATION_ATTENTION_LIMIT, PRESENTATION_CAPABILITY,
    PRESENTATION_PAGE_SIZE, PRESENTATION_RUNS_CAPABILITY, PresentationAddress, PresentationClient,
    run, run_for_endpoint,
};
#[cfg(windows)]
pub use ipc::{WEZTERM_CAPABILITY, WEZTERM_SELECTION_CAPABILITY};
#[cfg(windows)]
pub use platform::{WezTermCandidate, discover_wezterm_panes};
pub use slots::{MAX_SLOTS, MAX_TRACKED, RunKey, RunObservation, Slot, SlotLayout};
pub use store::{Config, HostEndpoint, Recovery, Store, initialize};
