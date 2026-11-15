use std::process::ExitCode;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod linux;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() -> ExitCode {
    linux::run_main()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() -> ExitCode {
    eprintln!("the terminal qualification spike requires Linux or macOS");
    ExitCode::FAILURE
}
