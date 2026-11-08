mod ipc;
mod last_known;
mod platform;
mod remote;
mod service;
mod slots;
mod store;

#[cfg(windows)]
pub use ipc::WEZTERM_CAPABILITY;
pub use ipc::{
    PRESENTATION_CAPABILITY, PRESENTATION_PAGE_SIZE, PresentationAddress, PresentationClient, run,
};
pub use slots::{MAX_SLOTS, RunKey, RunObservation, Slot, SlotLayout};
pub use store::{Config, HostEndpoint, Recovery, Store, initialize};
