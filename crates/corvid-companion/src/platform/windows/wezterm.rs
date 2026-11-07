use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, AsRawSocket, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::Stdio;
use std::ptr::{null, null_mut};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_protocol::wire::{LocalActivation, WezTermLocation, WezTermTarget};
use serde::{Deserialize, Serialize};
use socket2::{Domain, SockAddr, Socket, Type};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;
use windows_sys::Win32::Foundation::{FILETIME, HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::Networking::WinSock::{
    SIO_AF_UNIX_GETPEERPID, SOCKET_ERROR, WSAGetLastError, WSAIoctl,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_READ_DATA, FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo, FileIdInfo,
    GetFileInformationByHandleEx,
};
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_AF_UNIX;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, WaitForSingleObject,
};

use super::security::{checked, current_logon, current_user, process_logon, process_user};
use super::storage::{check_component, require_local_acl_volume};

mod foreground;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const CLI_TIMEOUT: Duration = Duration::from_secs(2);
const LIST_LIMIT: u64 = 1024 * 1024;

pub(crate) struct WezTermPane {
    gui: Gui,
    location: PaneLocation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct WezTermRegistration {
    gui: GuiIdentity,
    location: PaneLocation,
}

impl WezTermRegistration {
    pub(crate) fn location(&self) -> WezTermLocation {
        self.location.wire(self.gui.pid)
    }

    pub(crate) fn valid(&self) -> bool {
        self.gui.created != 0
            && self.gui.file.id != [0; 16]
            && self.gui.socket.is_absolute()
            && self
                .gui
                .socket
                .to_str()
                .is_some_and(|path| path.len() < 108)
            && socket_pid(&self.gui.socket).is_ok_and(|pid| pid == self.gui.pid)
    }
}

impl WezTermPane {
    pub(crate) async fn capture(target: WezTermTarget) -> Result<WezTermRegistration> {
        let pane = target.pane_id.context("missing WezTerm pane ID")?;
        ensure!(
            target.gui_socket.len() < 108,
            "WezTerm socket path exceeds the AF_UNIX limit"
        );
        let gui = Gui::open(Path::new(&target.gui_socket))?;
        let location = find_pane(&gui.panes().await?, pane)?;
        Ok(WezTermRegistration {
            gui: gui.identity,
            location,
        })
    }

    pub(crate) async fn restore(saved: &WezTermRegistration) -> Result<Self> {
        let gui = Gui::open(&saved.gui.socket)?;
        ensure!(
            gui.identity == saved.gui,
            "WezTerm GUI process or socket was replaced"
        );
        let pane = Self {
            gui,
            location: saved.location,
        };
        pane.check().await?;
        Ok(pane)
    }

    pub(crate) async fn check(&self) -> Result<()> {
        self.location.verify(&self.gui.panes().await?)
    }

    pub(crate) fn location(&self) -> WezTermLocation {
        self.location.wire(self.gui.identity.pid)
    }

    pub(crate) async fn activate(&self) -> LocalActivation {
        if self.activate_pane().await.is_err() {
            return LocalActivation::Unavailable;
        }

        match self.activate_foreground().await {
            Ok(true) => LocalActivation::Activated,
            _ => LocalActivation::PaneSelectedForegroundUnknown,
        }
    }

    async fn activate_pane(&self) -> Result<()> {
        self.check().await?;
        self.gui
            .invoke(&[
                "activate-pane",
                "--pane-id",
                &self.location.pane_id.to_string(),
            ])
            .await?;
        Ok(())
    }

    async fn activate_foreground(&self) -> Result<bool> {
        let panes = self.gui.panes().await?;
        self.location.verify(&panes)?;
        // The mux window ID and HWND are different namespaces. Only attempt OS
        // activation when both inventories identify a single window.
        if panes
            .iter()
            .any(|pane| pane.window_id != self.location.window_id)
        {
            return Ok(false);
        }
        foreground::activate(self.gui.identity.pid, &self.gui.process).await
    }
}

struct Gui {
    identity: GuiIdentity,
    process: OwnedHandle,
    executable: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct GuiIdentity {
    pid: u32,
    created: u64,
    socket: PathBuf,
    file: SocketIdentity,
}

impl Gui {
    fn open(socket: &Path) -> Result<Self> {
        let socket = socket_path(socket)?;
        let pid = socket_pid(&socket)?;
        // The handle grants query and wait rights to the specified process.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        ensure!(!process.is_null(), "WezTerm GUI process is unavailable");
        // OpenProcess returned a new owned handle.
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        let handle = process.as_raw_handle();
        ensure!(
            process_user(handle)? == current_user()?,
            "WezTerm GUI belongs to another user"
        );
        ensure!(
            process_logon(handle)? == current_logon()?,
            "WezTerm GUI belongs to another Windows logon"
        );
        let image = process_image(handle)?;
        ensure!(
            image
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("wezterm-gui.exe")),
            "socket does not identify a WezTerm GUI process"
        );
        let created = process_creation(handle)?;
        let guards = pin_socket(&socket)?;
        let file = socket_identity(&guards)?;
        Ok(Self {
            identity: GuiIdentity {
                pid,
                created,
                socket,
                file,
            },
            process,
            executable: image.with_file_name("wezterm.exe"),
        })
    }

    async fn panes(&self) -> Result<Vec<PaneLocation>> {
        let bytes = self.invoke(&["list", "--format", "json"]).await?;
        serde_json::from_slice(&bytes).context("invalid WezTerm pane inventory")
    }

    async fn invoke(&self, args: &[&str]) -> Result<Vec<u8>> {
        let socket = &self.identity.socket;
        let path_guard = pin_socket(socket)?;
        ensure!(
            socket_identity(&path_guard)? == self.identity.file,
            "WezTerm GUI socket was replaced"
        );
        ensure!(
            process_alive(&self.process)
                && peer_pid(socket)? == self.identity.pid
                && process_alive(&self.process),
            "cannot verify the WezTerm GUI socket peer"
        );

        let mut child = Command::new(&self.executable)
            .args(["--skip-config", "cli", "--no-auto-start"])
            .args(args)
            .env("WEZTERM_UNIX_SOCKET", socket)
            .env_remove("WEZTERM_PANE")
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting installed WezTerm CLI")?;
        let mut stdout = child.stdout.take().context("missing WezTerm CLI output")?;
        let bytes = timeout(CLI_TIMEOUT, async {
            let mut bytes = Vec::new();
            (&mut stdout)
                .take(LIST_LIMIT + 1)
                .read_to_end(&mut bytes)
                .await?;
            ensure!(
                bytes.len() as u64 <= LIST_LIMIT,
                "WezTerm CLI output exceeds limit"
            );
            ensure!(child.wait().await?.success(), "WezTerm CLI failed");
            Ok::<_, anyhow::Error>(bytes)
        })
        .await
        .context("WezTerm CLI timed out")??;
        ensure!(process_alive(&self.process), "WezTerm GUI exited");
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PaneLocation {
    window_id: u32,
    tab_id: u32,
    pane_id: u32,
}

impl PaneLocation {
    fn wire(self, pid: u32) -> WezTermLocation {
        WezTermLocation {
            gui_pid: pid,
            window_id: self.window_id,
            tab_id: self.tab_id,
            pane_id: self.pane_id,
        }
    }

    fn verify(self, panes: &[Self]) -> Result<()> {
        ensure!(
            find_pane(panes, self.pane_id)? == self,
            "registered WezTerm pane moved"
        );
        Ok(())
    }
}

fn find_pane(panes: &[PaneLocation], pane_id: u32) -> Result<PaneLocation> {
    let mut matches = panes.iter().filter(|pane| pane.pane_id == pane_id);
    let location = matches.next().context("WezTerm pane is absent")?;
    ensure!(matches.next().is_none(), "ambiguous WezTerm pane ID");
    Ok(*location)
}

fn process_image(process: HANDLE) -> Result<PathBuf> {
    let mut name = vec![0; 32_768];
    let mut len = name.len() as u32;
    // The buffer and size argument are writable for the duration of the call.
    checked(unsafe { QueryFullProcessImageNameW(process, 0, name.as_mut_ptr(), &mut len) })?;
    let path = PathBuf::from(String::from_utf16(&name[..len as usize])?);
    ensure!(
        path.is_absolute(),
        "WezTerm executable path is not absolute"
    );
    Ok(path)
}

fn process_alive(process: &OwnedHandle) -> bool {
    // A process handle becomes signalled when that process exits.
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_TIMEOUT }
}

fn process_creation(process: HANDLE) -> Result<u64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // The live process handle grants query rights and all results are writable.
    checked(unsafe {
        GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user)
    })?;
    let timestamp = u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime);
    ensure!(timestamp != 0, "missing WezTerm GUI process creation time");
    Ok(timestamp)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SocketIdentity {
    volume: u64,
    id: [u8; 16],
}

fn socket_identity(guards: &[File]) -> Result<SocketIdentity> {
    let socket = guards.last().context("missing guarded WezTerm socket")?;
    let mut info = FILE_ID_INFO::default();
    // The last handle is the checked reparse point, with read attributes access.
    checked(unsafe {
        GetFileInformationByHandleEx(
            socket.as_raw_handle(),
            FileIdInfo,
            std::ptr::from_mut(&mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    })?;
    ensure!(
        info.FileId.Identifier != [0; 16],
        "missing WezTerm socket file ID"
    );
    Ok(SocketIdentity {
        volume: info.VolumeSerialNumber,
        id: info.FileId.Identifier,
    })
}

fn socket_path(path: &Path) -> Result<PathBuf> {
    let mut parts = path.components();
    let Some(Component::Prefix(prefix)) = parts.next() else {
        anyhow::bail!("WezTerm GUI socket requires an absolute local path");
    };
    let Prefix::Disk(drive) = prefix.kind() else {
        anyhow::bail!("WezTerm GUI socket requires an absolute local path");
    };
    ensure!(
        parts.next() == Some(Component::RootDir),
        "relative WezTerm GUI socket"
    );
    let mut socket = PathBuf::from(format!("{}:\\", char::from(drive)));
    for component in parts {
        let Component::Normal(name) = component else {
            anyhow::bail!("WezTerm socket path contains traversal");
        };
        check_component(name)?;
        socket.push(name);
    }
    SockAddr::unix(&socket)?;
    require_local_acl_volume(drive)?;
    Ok(socket)
}

fn socket_pid(path: &Path) -> Result<u32> {
    let pid = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("gui-sock-"))
        .context("expected a per-process WezTerm GUI socket")?;
    let number = pid.parse::<u32>().context("invalid WezTerm GUI PID")?;
    ensure!(
        number > 0 && number.to_string() == pid,
        "invalid WezTerm GUI PID"
    );
    Ok(number)
}

fn pin_socket(socket: &Path) -> Result<Vec<File>> {
    let mut guards = Vec::new();
    let mut components = socket.components().peekable();
    let mut path = PathBuf::new();
    while let Some(component) = components.next() {
        path.push(component.as_os_str());
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        let directory = components.peek().is_some();
        // The handles deny delete sharing, so the checked socket or its
        // ancestors cannot be replaced while the CLI opens its connection.
        let file = OpenOptions::new()
            .access_mode(FILE_READ_DATA | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)?;
        check_socket_node(&file, directory)?;
        guards.push(file);
    }
    Ok(guards)
}

fn check_socket_node(file: &File, directory: bool) -> Result<()> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // Windows fills the writable information structure for this live handle.
    checked(unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            std::ptr::from_mut(&mut info).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    })?;
    ensure!(
        (info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) == directory,
        "wrong file type in WezTerm socket path"
    );
    if directory {
        ensure!(
            info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "WezTerm socket path traverses a reparse point"
        );
    } else {
        ensure!(
            info.ReparseTag == IO_REPARSE_TAG_AF_UNIX,
            "expected a Windows AF_UNIX socket"
        );
    }
    Ok(())
}

fn peer_pid(path: &Path) -> Result<u32> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.connect_timeout(&SockAddr::unix(path)?, CONNECT_TIMEOUT)?;
    let mut pid = 0u32;
    let mut len = 0u32;
    // The connected socket is live and the result buffers hold one DWORD each.
    let result = unsafe {
        WSAIoctl(
            socket.as_raw_socket() as usize,
            SIO_AF_UNIX_GETPEERPID,
            null(),
            0,
            std::ptr::from_mut(&mut pid).cast(),
            size_of::<u32>() as u32,
            &mut len,
            null_mut(),
            None,
        )
    };
    if result == SOCKET_ERROR {
        // WSAGetLastError returns the error from WSAIoctl on this thread.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }))
            .context("querying socket peer PID");
    }
    ensure!(
        len as usize == size_of::<u32>() && pid != 0,
        "unverified socket peer PID"
    );
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_exact_gui_socket_pid_names() {
        assert_eq!(socket_pid(Path::new("gui-sock-42")).unwrap(), 42);
        for path in [
            "sock",
            "gui-sock-0",
            "gui-sock-042",
            "gui-sock-+42",
            "gui-sock-42.bak",
        ] {
            assert!(socket_pid(Path::new(path)).is_err(), "{path}");
        }
    }

    #[test]
    fn selects_the_unique_numeric_pane_and_ignores_titles() {
        let panes = serde_json::from_str::<Vec<PaneLocation>>(
            r#"[
            {"window_id":1,"tab_id":5,"pane_id":9,"title":"same"},
            {"window_id":2,"tab_id":6,"pane_id":0,"title":"same"}
        ]"#,
        )
        .unwrap();
        assert_eq!(
            find_pane(&panes, 0).unwrap(),
            PaneLocation {
                window_id: 2,
                tab_id: 6,
                pane_id: 0
            }
        );
        assert!(find_pane(&panes, 2).is_err());
        assert!(find_pane(&[panes[0], panes[0]], 9).is_err());
        let moved = PaneLocation {
            tab_id: 42,
            ..panes[0]
        };
        assert!(panes[0].verify(&panes).is_ok());
        assert!(panes[0].verify(&[moved]).is_err());
    }

    #[test]
    fn identifies_a_live_windows_af_unix_peer_and_pins_the_endpoint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sock");
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        listener.bind(&SockAddr::unix(&path).unwrap()).unwrap();
        listener.listen(2).unwrap();
        let guard = pin_socket(&path).unwrap();
        let id = socket_identity(&guard).unwrap();

        assert_eq!(peer_pid(&path).unwrap(), std::process::id());
        assert_eq!(socket_identity(&pin_socket(&path).unwrap()).unwrap(), id);
        assert!(std::fs::rename(&path, directory.path().join("renamed")).is_err());
        drop(guard);
        drop(listener);
    }

    #[test]
    fn creation_time_and_file_identity_distinguish_windows_objects() {
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        // This pseudo-handle is valid for process and token queries.
        let current = unsafe { GetCurrentProcess() };
        assert_eq!(current_logon().unwrap(), process_logon(current).unwrap());
        assert_eq!(
            process_creation(current).unwrap(),
            process_creation(current).unwrap()
        );

        let first = [tempfile::tempfile().unwrap()];
        let replacement = [tempfile::tempfile().unwrap()];
        assert_ne!(
            socket_identity(&first).unwrap(),
            socket_identity(&replacement).unwrap()
        );
    }
}
