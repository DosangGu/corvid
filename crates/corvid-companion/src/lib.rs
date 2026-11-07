mod ipc;
mod platform;
mod remote;
mod service;
mod slots;
mod store;

pub use ipc::{PRESENTATION_CAPABILITY, PresentationAddress, PresentationClient, run};
pub use slots::{MAX_SLOTS, RunKey, RunObservation, Slot, SlotLayout};
pub use store::{Config, HostEndpoint, Recovery, Store, initialize};
