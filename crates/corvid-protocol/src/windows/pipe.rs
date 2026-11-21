use std::os::windows::io::AsRawHandle;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

use super::security::{PrivateDescriptor, current_user, verify_private};

const COMPANION_PREFIX: &str = "corvid-v1";

#[cfg(feature = "compio")]
#[path = "compio_pipe.rs"]
mod compio_pipe;

#[cfg(feature = "compio")]
pub use compio_pipe::{CompioPipeListener, child_stdio_pipe};

/// Connect to the private companion presentation pipe using Compio.
#[cfg(feature = "compio")]
pub async fn open_compio_pipe(instance: &str) -> Result<compio::fs::named_pipe::NamedPipeClient> {
    open_scoped(COMPANION_PREFIX, instance).await
}

#[cfg(feature = "compio")]
pub async fn open_scoped(
    prefix: &str,
    instance: &str,
) -> Result<compio::fs::named_pipe::NamedPipeClient> {
    let user = current_user()?;
    let name = pipe_name(prefix, &user, instance)?;
    let mut options = compio::fs::named_pipe::ClientOptions::new();
    options.security_qos_flags(SECURITY_IDENTIFICATION);
    let pipe = loop {
        match options.open(&name).await {
            Ok(pipe) => break pipe,
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                compio::time::sleep(Duration::from_millis(2)).await;
            }
            Err(error) => return Err(error).context("connecting to private pipe"),
        }
    };
    verify_private(pipe.as_raw_handle(), &user, false)?;
    Ok(pipe)
}

fn pipe_name(prefix: &str, user: &str, instance: &str) -> Result<String> {
    ensure!(
        (1..=32).contains(&instance.len())
            && instance
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "pipe instance must use 1–32 ASCII letters, digits, hyphens, or underscores"
    );
    let instance = instance.to_ascii_lowercase();
    let name = format!(r"\\.\pipe\{prefix}-{user}-{instance}");
    ensure!(name.len() < 256, "pipe name exceeds Windows limit");
    Ok(name)
}

#[cfg(all(test, feature = "compio"))]
mod tests {
    use super::*;
    use compio::buf::BufResult;
    use compio::fs::named_pipe::ServerOptions;
    use compio::io::{AsyncReadExt, AsyncWriteExt};
    use uuid::Uuid;

    #[compio::test]
    async fn exchanges_bytes_over_the_private_pipe() {
        let instance = Uuid::new_v4().simple().to_string();
        let mut listener = CompioPipeListener::bind(&instance).unwrap();
        let mut client = open_compio_pipe(&instance).await.unwrap();
        let mut server = listener.accept().await.unwrap();

        let BufResult(result, _) = client.write_all(b"corvid").await;
        result.unwrap();
        let BufResult(result, message) = server.read_exact([0; 6]).await;
        result.unwrap();
        assert_eq!(&message, b"corvid");
    }

    #[compio::test]
    async fn refuses_a_second_listener_for_the_same_pipe() {
        let instance = Uuid::new_v4().simple().to_string();
        let _listener = CompioPipeListener::bind(&instance).unwrap();
        assert!(CompioPipeListener::bind(&instance).is_err());
    }

    #[compio::test]
    async fn rejects_a_pipe_with_a_default_security_descriptor() {
        let user = current_user().unwrap();
        let instance = Uuid::new_v4().simple().to_string();
        let name = pipe_name(COMPANION_PREFIX, &user, &instance).unwrap();
        let _untrusted = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        assert!(open_compio_pipe(&instance).await.is_err());
        assert!(CompioPipeListener::bind(&instance).is_err());
    }
}
