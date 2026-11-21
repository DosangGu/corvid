//! Synchronization shared by Corvid's async workers and blocking PTY threads.

pub mod mpsc;
pub mod oneshot;
pub mod watch;

pub use async_lock::{Mutex, Semaphore, SemaphoreGuard};
