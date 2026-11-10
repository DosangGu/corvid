use std::process::ExitCode;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    linux::run_main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("the terminal qualification spike requires Linux");
    ExitCode::FAILURE
}
