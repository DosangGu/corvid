use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), Box<dyn Error>> {
    if env::var("CARGO_CFG_TARGET_OS")? != "linux" {
        return Ok(());
    }

    let profile = "../../spikes/terminal/corvid.terminfo";
    println!("cargo:rerun-if-changed={profile}");
    let output = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing Cargo OUT_DIR")?);
    let status = Command::new("tic")
        .args(["-x", "-o"])
        .arg(output.join("terminfo"))
        .arg(profile)
        .status()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("running tic to compile terminfo: {error}"),
            )
        })?;
    if !status.success() {
        return Err(
            io::Error::other(format!("compiling Corvid terminfo failed ({status})")).into(),
        );
    }
    Ok(())
}
