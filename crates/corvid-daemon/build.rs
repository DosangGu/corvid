use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=assets");
    if !matches!(env::var("CARGO_CFG_TARGET_OS")?.as_str(), "linux" | "macos") {
        return Ok(());
    }

    let profile = "assets/corvid.terminfo";
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
    // macOS tic uses hexadecimal directory names; embed one stable build path.
    let compiled = ["c/corvid", "63/corvid"]
        .into_iter()
        .map(|name| output.join("terminfo").join(name))
        .find(|path| path.is_file())
        .ok_or("tic did not generate the corvid profile")?;
    std::fs::copy(compiled, output.join("corvid.terminfo"))?;
    Ok(())
}
