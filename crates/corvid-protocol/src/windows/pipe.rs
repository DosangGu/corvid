use std::os::windows::io::AsRawHandle;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::time::sleep;
use windows_sys::Win32::Foundation::{ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED};
use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

use super::security::{PrivateDescriptor, current_user, verify_private};

const COMPANION_PREFIX: &str = "corvid-v1";

pub struct PipeListener {
    name: String,
    user: String,
    waiting: NamedPipeServer,
}

impl PipeListener {
    pub fn bind(instance: &str) -> Result<Self> {
        Self::bind_scoped(COMPANION_PREFIX, instance)
    }

    pub(super) fn bind_scoped(prefix: &str, instance: &str) -> Result<Self> {
        let user = current_user()?;
        let name = pipe_name(prefix, &user, instance)?;
        let waiting = listen(&name, &user, true)?;
        Ok(Self {
            name,
            user,
            waiting,
        })
    }

    pub async fn accept(&mut self) -> Result<NamedPipeServer> {
        loop {
            let result = self.waiting.connect().await;
            // Keep an instance listening while the accepted connection is in use.
            let next = listen(&self.name, &self.user, false)?;
            let accepted = std::mem::replace(&mut self.waiting, next);
            match result {
                Ok(()) => return Ok(accepted),
                Err(error)
                    if matches!(
                        error.raw_os_error().map(|code| code as u32),
                        Some(ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED)
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn listen(name: &str, user: &str, first: bool) -> Result<NamedPipeServer> {
    let descriptor = PrivateDescriptor::new(user, false)?;
    let mut attributes = descriptor.attributes();
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    // The descriptor and attributes remain live during CreateNamedPipeW.
    let pipe = unsafe {
        options
            .create_with_security_attributes_raw(name, std::ptr::from_mut(&mut attributes).cast())
    }
    .context("opening private pipe")?;
    verify_private(pipe.as_raw_handle(), user, false).context("verifying pipe ACL")?;
    Ok(pipe)
}

pub async fn open_pipe(instance: &str) -> Result<NamedPipeClient> {
    open_scoped(COMPANION_PREFIX, instance).await
}

pub(super) async fn open_scoped(prefix: &str, instance: &str) -> Result<NamedPipeClient> {
    let user = current_user()?;
    let name = pipe_name(prefix, &user, instance)?;
    let mut options = ClientOptions::new();
    options.security_qos_flags(SECURITY_IDENTIFICATION);
    let pipe = loop {
        match options.open(&name) {
            Ok(pipe) => break pipe,
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                sleep(Duration::from_millis(25)).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use uuid::Uuid;

    #[tokio::test]
    async fn exchanges_bytes_over_the_private_pipe() {
        let instance = Uuid::new_v4().simple().to_string();
        let mut listener = PipeListener::bind(&instance).unwrap();
        let mut client = open_pipe(&instance).await.unwrap();
        let mut server = listener.accept().await.unwrap();

        client.write_all(b"corvid").await.unwrap();
        let mut message = [0; 6];
        server.read_exact(&mut message).await.unwrap();
        assert_eq!(&message, b"corvid");
    }

    #[tokio::test]
    async fn refuses_a_second_listener_for_the_same_pipe() {
        let instance = Uuid::new_v4().simple().to_string();
        let _listener = PipeListener::bind(&instance).unwrap();
        assert!(PipeListener::bind(&instance).is_err());
    }

    #[tokio::test]
    async fn rejects_a_pipe_with_a_default_security_descriptor() {
        let user = current_user().unwrap();
        let instance = Uuid::new_v4().simple().to_string();
        let name = pipe_name(COMPANION_PREFIX, &user, &instance).unwrap();
        let _untrusted = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        assert!(open_pipe(&instance).await.is_err());
        assert!(PipeListener::bind(&instance).is_err());
    }
}
