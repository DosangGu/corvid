//! Self-contained process/pipe/Win32 regression fixture. No GPU or user input.
#[cfg(windows)]
#[path = "support/update_windows.rs"]
mod native;

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    native::run()?;
    Ok(())
}
