#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use std::path::PathBuf;

    let mut arguments = std::env::args_os().skip(1);
    let path = arguments
        .next()
        .map(PathBuf::from)
        .context("usage: compio-ping <runtime-directory>")?;
    anyhow::ensure!(arguments.next().is_none(), "expected one runtime directory");

    let runtime = compio::runtime::Runtime::new()?;
    runtime.block_on(corvid_client::compio_ping(&path))?;
    println!("pong");
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn main() {
    eprintln!("Compio IPC probe requires Linux, macOS, or Windows.");
    std::process::exit(1);
}
