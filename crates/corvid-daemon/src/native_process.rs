use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use corvid_core::agent::{ProcessLiveness, Provider};
use windows_sys::Win32::Foundation::{FILETIME, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, WaitForSingleObject,
};

const MAX_PROCESSES: usize = 8192;
const MAX_CANDIDATES: usize = 128;
const MAX_ANCESTORS: usize = 24;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ProcessKey {
    pub pid: u32,
    pub created: u64,
}

impl ProcessKey {
    pub fn read(pid: u32, handle: RawHandle) -> Option<Self> {
        let mut created = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // The caller keeps the process handle alive for the query.
        if unsafe { GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user) } == 0
        {
            return None;
        }
        let created = u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime);
        (pid != 0 && created != 0).then_some(Self { pid, created })
    }
}

pub(crate) struct Process {
    pub key: ProcessKey,
    handle: OwnedHandle,
}

impl Process {
    pub fn open(pid: u32) -> Option<Self> {
        // A successful OpenProcess transfers ownership of a process handle.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return None;
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let key = ProcessKey::read(pid, handle.as_raw_handle())?;
        Some(Self { key, handle })
    }

    pub fn liveness(&self) -> ProcessLiveness {
        let handle = self.handle.as_raw_handle();
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_TIMEOUT => ProcessLiveness::Running,
            WAIT_OBJECT_0 => {
                let mut code = 0;
                let known = unsafe { GetExitCodeProcess(handle, &mut code) } != 0;
                ProcessLiveness::Exited {
                    code: known.then_some(code),
                }
            }
            _ => ProcessLiveness::Unknown,
        }
    }

    fn image(&self) -> Option<std::path::PathBuf> {
        let mut path = [0; 4096];
        let mut len = path.len() as u32;
        if unsafe {
            QueryFullProcessImageNameW(self.handle.as_raw_handle(), 0, path.as_mut_ptr(), &mut len)
        } == 0
        {
            return None;
        }
        Some(OsString::from_wide(&path[..len as usize]).into())
    }

    fn provider(&self) -> Option<Provider> {
        provider_name(self.image()?.file_name()?.to_str()?)
    }
}

struct Entry {
    parent: u32,
    provider: Option<Provider>,
}

pub(crate) struct Found {
    pub root: ProcessKey,
    pub provider: Provider,
    pub process: Process,
}

pub(crate) struct HookOwner {
    pub found: Found,
    pub producer: ProcessKey,
}

pub(crate) fn verify_hook(pid: u32, root: ProcessKey, provider: Provider) -> Option<HookOwner> {
    let as_of = file_time_now()?;
    let entries = snapshot()?;
    let peer = Process::open(pid)?;
    if peer.key.created > as_of || !is_helper(&peer.image()?) {
        return None;
    }
    let mut parent = parent(&entries, peer.key)?;
    if provider == Provider::Codex && is_hook_shell(&parent.image()?) {
        parent = self::parent(&entries, parent.key)?;
    }
    if parent.provider() != Some(provider) {
        return None;
    }
    let producer = parent.key;
    let mut owner = parent;
    let mut next = owner.key;
    for _ in 0..MAX_ANCESTORS {
        if next == root {
            return Some(HookOwner {
                found: Found {
                    root,
                    provider,
                    process: owner,
                },
                producer,
            });
        }
        let parent = self::parent(&entries, next)?;
        next = parent.key;
        if parent.provider() == Some(provider) {
            owner = parent;
        }
    }
    None
}

fn is_helper(path: &Path) -> bool {
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    let Some(current) = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
    else {
        return false;
    };
    path == current
        || (current
            .file_stem()
            .is_some_and(|name| name == "corvid-desktop")
            && path == current.with_file_name("corvid.exe"))
}

fn is_hook_shell(path: &Path) -> bool {
    let Some(system) = std::env::var_os("SystemRoot") else {
        return false;
    };
    let shell = Path::new(&system).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    path.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&shell.as_os_str().to_string_lossy())
}

fn parent(entries: &BTreeMap<u32, Entry>, child: ProcessKey) -> Option<Process> {
    let process = Process::open(entries.get(&child.pid)?.parent)?;
    (process.key.created <= child.created && process.liveness() == ProcessLiveness::Running)
        .then_some(process)
}

pub(crate) fn scan(roots: &[ProcessKey]) -> Vec<Found> {
    if roots.is_empty() {
        return Vec::new();
    }
    let Some(started) = file_time_now() else {
        return Vec::new();
    };
    let Some(entries) = snapshot() else {
        return Vec::new();
    };
    let roots: BTreeMap<_, _> = roots.iter().map(|root| (root.pid, *root)).collect();
    entries
        .iter()
        .filter_map(|(&pid, entry)| entry.provider.map(|provider| (pid, provider)))
        .take(MAX_CANDIDATES)
        .filter_map(|(pid, provider)| {
            let process = Process::open(pid)?;
            if process.key.created > started
                || process.liveness() != ProcessLiveness::Running
                || process.provider() != Some(provider)
            {
                return None;
            }
            let root = ancestor(&entries, &roots, process.key, provider)?;
            Some(Found {
                root,
                provider,
                process,
            })
        })
        .collect()
}

fn file_time_now() -> Option<u64> {
    const UNIX_EPOCH_FILE_TIME: u64 = 116_444_736_000_000_000;
    let ticks = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos()
            / 100,
    )
    .ok()?;
    UNIX_EPOCH_FILE_TIME.checked_add(ticks)
}

fn ancestor(
    entries: &BTreeMap<u32, Entry>,
    roots: &BTreeMap<u32, ProcessKey>,
    mut child: ProcessKey,
    provider: Provider,
) -> Option<ProcessKey> {
    for _ in 0..MAX_ANCESTORS {
        let pid = entries.get(&child.pid)?.parent;
        let entry = entries.get(&pid)?;
        let parent = parent(entries, child)?;
        // A reused parent PID cannot own a process created before that parent.
        if let Some(root) = roots.get(&pid) {
            return (*root == parent.key).then_some(*root);
        }
        if entry.provider == Some(provider) && parent.provider() == Some(provider) {
            return None;
        }
        child = parent.key;
    }
    None
}

fn snapshot() -> Option<BTreeMap<u32, Entry>> {
    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut entries = BTreeMap::new();
    let mut present = unsafe { Process32FirstW(handle.as_raw_handle(), &mut entry) } != 0;
    while present {
        if entries.len() >= MAX_PROCESSES {
            return None;
        }
        let name = entry
            .szExeFile
            .split(|&unit| unit == 0)
            .next()
            .unwrap_or_default();
        let provider = String::from_utf16(name)
            .ok()
            .and_then(|name| provider_name(&name));
        entries.insert(
            entry.th32ProcessID,
            Entry {
                parent: entry.th32ParentProcessID,
                provider,
            },
        );
        present = unsafe { Process32NextW(handle.as_raw_handle(), &mut entry) } != 0;
    }
    Some(entries)
}

fn provider_name(name: &str) -> Option<Provider> {
    if name.eq_ignore_ascii_case("codex.exe") {
        Some(Provider::Codex)
    } else if name.eq_ignore_ascii_case("claude.exe") {
        Some(Provider::Claude)
    } else {
        None
    }
}
