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
    let newer = release.is_newer_than(current)?;
    if options.check {
        println!(
            "{}",
            if newer {
                "An update is available."
            } else {
                "No newer release is available."
            }
        );
        return Ok(0);
    }
    if options.version.is_none() && !newer {
        println!("Corvid is up to date.");
        return Ok(0);
    }
    let installation = corvid_update::Installation::current()?;
    let installed = installation.install_with_progress(&release, |stage| {
        eprintln!(
            "{}",
            match stage {
                corvid_update::InstallProgress::Downloading => "Downloading update…",
                corvid_update::InstallProgress::Verifying => "Verifying download…",
                corvid_update::InstallProgress::Installing => "Installing update…",
            }
        );
    })?;
    print!("{}", installed.message);
    println!(
        "Update installed. Reopen Corvid when you are ready. Running terminal jobs continue; server updates are separate."
    );
    Ok(0)
}
