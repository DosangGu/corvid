use std::env;
use std::error::Error;
use std::path::Path;
use std::process::{Command, ExitCode};

const USAGE: &str = "Usage: cargo build-desktop [WSL-DISTRIBUTION] (default: Ubuntu)";

fn main() -> Result<ExitCode, Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let distribution = args.next().unwrap_or_else(|| "Ubuntu".into());
    if args.next().is_some() {
        return Err(USAGE.into());
    }
    if distribution == "--help" || distribution == "-h" {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if !cfg!(windows) {
        return Err("Build the Desktop distribution on Windows with WSL installed".into());
    }

    let packager = Path::new(env!("CARGO_MANIFEST_DIR")).join("../windows/Package-Corvid.ps1");
    let status = Command::new("powershell.exe")
        // Use Windows PowerShell's modules even when Cargo was launched from PowerShell 7.
        .env_remove("PSModulePath")
        .args(["-NoProfile", "-File"])
        .arg(packager)
        .arg("-WslDistribution")
        .arg(distribution)
        .status()?;
    let code = status.code().and_then(|code| u8::try_from(code).ok());
    Ok(ExitCode::from(code.unwrap_or(1)))
}
