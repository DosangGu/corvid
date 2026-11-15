use std::env;
use std::error::Error;
use std::path::Path;
use std::process::{Command, ExitCode};

const USAGE: &str = "Usage: cargo build-desktop [WSL-DISTRIBUTION] (Windows; set CORVID_MACOS_RUNTIME), or cargo build-desktop LINUX_RUNTIME WINDOWS_RUNTIME (macOS)";

fn main() -> Result<ExitCode, Box<dyn Error>> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let deploy = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut command = if cfg!(target_os = "macos") {
        if args.len() != 2 {
            return Err(USAGE.into());
        }
        let mut command = Command::new("bash");
        command
            .arg(deploy.join("macos/package-corvid.sh"))
            .args(args);
        command
    } else if cfg!(windows) {
        if args.len() > 1 {
            return Err(USAGE.into());
        }
        let distribution = args.first().cloned().unwrap_or_else(|| "Ubuntu".into());
        let macos = env::var_os("CORVID_MACOS_RUNTIME").ok_or(
            "Set CORVID_MACOS_RUNTIME to the revision-matched macOS runtime artifact directory",
        )?;
        let mut command = Command::new("powershell.exe");
        // Use Windows PowerShell modules even when Cargo was launched from PowerShell 7.
        command
            .env_remove("PSModulePath")
            .args(["-NoProfile", "-File"])
            .arg(deploy.join("windows/Package-Corvid.ps1"))
            .arg("-MacosDaemonDirectory")
            .arg(macos)
            .arg("-WslDistribution")
            .arg(distribution);
        command
    } else {
        return Err("Build the Desktop distribution on macOS or Windows with WSL installed".into());
    };
    let status = command.status()?;
    let code = status.code().and_then(|code| u8::try_from(code).ok());
    Ok(ExitCode::from(code.unwrap_or(1)))
}
