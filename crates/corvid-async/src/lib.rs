//! Synchronization shared by Corvid's async workers and blocking PTY threads.

pub mod io;
pub mod mpsc;
pub mod oneshot;
pub mod task;
pub mod time;
pub mod watch;

pub use async_lock::{Mutex, Semaphore, SemaphoreGuard};

pub use futures_util::{join, pin_mut as pin, try_join};
pub use selectme::select;
