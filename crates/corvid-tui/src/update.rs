use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub(super) struct Options {
    /// Report the available version without installing it.
    #[arg(long)]
    check: bool,
    /// Select an exact release, including a prerelease or an older version.
    #[arg(long)]
    version: Option<String>,
}

pub(super) fn run(options: Options) -> Result<u32> {
    let release = corvid_update::check(options.version.as_deref())?;
    let current = env!("CARGO_PKG_VERSION");
    println!(
        "Installed: {current}\nAvailable: {}\n{}",
        release.version,
        release.url()
    );
    if options.check {
        return Ok(0);
    }
    if options.version.is_none() && !release.is_newer_than(current)? {
        println!("Corvid is up to date.");
        return Ok(0);
    }
    let installation = corvid_update::Installation::current()?;
    let installed = installation.install(&release)?;
    print!("{}", installed.message);
    Ok(0)
}
