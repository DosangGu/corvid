use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

pub fn command(program: &str) -> Command {
    let command = Command::new(program);
    #[cfg(windows)]
    let command = {
        use std::os::windows::process::CommandExt;
        let mut command = command;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        command
    };
    command
}

pub fn file(url: &str, destination: &Path, limit: u64) -> Result<()> {
    ensure!(url.starts_with("https://"), "downloads require HTTPS");
    let output = command(if cfg!(windows) { "curl.exe" } else { "curl" })
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "15",
            "--max-time",
            "300",
            "--retry",
            "2",
            "--user-agent",
            "corvid-updater",
            "--max-filesize",
        ])
        .arg(limit.to_string())
        .arg("--output")
        .arg(destination)
        .arg(url)
        .output()
        .context("starting curl; install curl to check for updates")?;
    ensure!(
        output.status.success(),
        "download failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    ensure!(
        fs::metadata(destination)?.len() <= limit,
        "download exceeds size limit"
    );
    Ok(())
}

pub fn bytes(url: &str, limit: u64) -> Result<Vec<u8>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("download");
    file(url, &path, limit)?;
    fs::read(path).context("reading download")
}

pub fn verify(bytes: &[u8], size: u64, sha256: &str) -> Result<()> {
    ensure!(bytes.len() as u64 == size, "download size mismatch");
    ensure!(
        format!("{:x}", Sha256::digest(bytes)) == sha256,
        "download SHA-256 mismatch"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_modified_downloads() {
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        super::verify(b"abc", 3, hash).unwrap();
        assert!(super::verify(b"abd", 3, hash).is_err());
        assert!(super::verify(b"abc", 4, hash).is_err());
    }
}
