#[cfg(windows)]
mod doctor;
mod ipc;
mod last_known;
#[cfg(windows)]
pub mod launch;
mod platform;
mod remote;
mod service;
mod slots;
mod store;

#[cfg(windows)]
pub use doctor::{Check, CheckStatus, diagnose};
pub use ipc::{
    PRESENTATION_ATTENTION_CAPABILITY, PRESENTATION_ATTENTION_LIMIT, PRESENTATION_CAPABILITY,
    PRESENTATION_PAGE_SIZE, PresentationAddress, PresentationClient, run, run_for_endpoint,
};
#[cfg(windows)]
pub use ipc::{WEZTERM_CAPABILITY, WEZTERM_SELECTION_CAPABILITY};
#[cfg(windows)]
pub use platform::{WezTermCandidate, discover_wezterm_panes};
pub use slots::{MAX_SLOTS, RunKey, RunObservation, Slot, SlotLayout};
pub use store::{Config, HostEndpoint, Recovery, Store, initialize};
