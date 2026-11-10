use std::collections::BTreeMap;
use std::io;
use std::ptr::{null, null_mut};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_BINARY, REG_OPTION_OPEN_LINK,
    REG_OPTION_VOLATILE, RRF_RT_REG_BINARY, RegCloseKey, RegCreateKeyExW, RegDeleteValueW,
    RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};

use super::wezterm::WezTermRegistration;
use corvid_protocol::windows::security::{
    PrivateDescriptor, current_logon, current_user, verify_private_registry,
};

const VERSION: u32 = 2;
const MAX_RECORDS: usize = 256;
const MAX_BYTES: u32 = 256 * 1024;

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TerminalRecord {
    pub(crate) epoch: Uuid,
    pub(crate) revision: u64,
    pub(crate) commit_sequence: u64,
    // Only an explicit null marks a manual pairing. A missing field is invalid.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub(crate) pane: Option<WezTermRegistration>,
}

impl TerminalRecord {
    pub(crate) fn matches(&self, epoch: Uuid, revision: u64, sequence: u64) -> bool {
        self.epoch == epoch && self.revision == revision && sequence >= self.commit_sequence
    }
}

#[derive(Deserialize, Serialize)]
struct SavedTerminals<T> {
    schema_version: u32,
    host: Uuid,
    records: T,
}

pub(crate) struct TerminalRegistry {
    path: Vec<u16>,
    name: Vec<u16>,
    user: String,
    host: Uuid,
}

impl TerminalRegistry {
    pub(crate) fn new(controller: Uuid, host: Uuid) -> Result<Self> {
        let logon = current_logon()?;
        let path = format!(r"Software\Corvid.WezTerm.LiveBindings.{logon:016x}");
        Self::at(&path, controller, host)
    }

    fn at(path: &str, controller: Uuid, host: Uuid) -> Result<Self> {
        Ok(Self {
            path: wide(path),
            name: wide(&controller.to_string()),
            user: current_user()?,
            host,
        })
    }

    pub(crate) fn load(&self) -> Result<BTreeMap<Uuid, TerminalRecord>> {
        let Some(key) = self.open(false)? else {
            return Ok(BTreeMap::new());
        };
        let mut size = 0;
        // The value name is NUL-terminated and the key remains open across both reads.
        let status = unsafe {
            RegGetValueW(
                key.0,
                null(),
                self.name.as_ptr(),
                RRF_RT_REG_BINARY,
                null_mut(),
                null_mut(),
                &mut size,
            )
        };
        if is_missing(status) {
            return Ok(BTreeMap::new());
        }
        status_ok(status)?;
        ensure!(
            (1..=MAX_BYTES).contains(&size),
            "invalid WezTerm registration size"
        );

        let mut bytes = vec![0; size as usize];
        // The result buffer is sized from the bounded length above.
        status_ok(unsafe {
            RegGetValueW(
                key.0,
                null(),
                self.name.as_ptr(),
                RRF_RT_REG_BINARY,
                null_mut(),
                bytes.as_mut_ptr().cast(),
                &mut size,
            )
        })?;
        bytes.truncate(size as usize);
        decode(&bytes, self.host)
    }

    pub(crate) fn save(&self, records: &BTreeMap<Uuid, TerminalRecord>) -> Result<()> {
        let bytes = encode(records, self.host)?;
        let Some(key) = self.open(!records.is_empty())? else {
            return Ok(());
        };
        if records.is_empty() {
            // The name is NUL-terminated and the handle grants write access.
            let status = unsafe { RegDeleteValueW(key.0, self.name.as_ptr()) };
            return if is_missing(status) {
                Ok(())
            } else {
                status_ok(status)
            };
        }

        // Publish the bounded metadata as one value in the volatile key.
        status_ok(unsafe {
            RegSetValueExW(
                key.0,
                self.name.as_ptr(),
                0,
                REG_BINARY,
                bytes.as_ptr(),
                bytes.len() as u32,
            )
        })
    }

    fn open(&self, create: bool) -> Result<Option<RegistryKey>> {
        let mut key = null_mut();
        let access = KEY_QUERY_VALUE | KEY_SET_VALUE | READ_CONTROL;
        // Only open the registry key itself and keep its handle local to this operation.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                self.path.as_ptr(),
                REG_OPTION_OPEN_LINK,
                access,
                &mut key,
            )
        };
        if is_missing(status) && !create {
            return Ok(None);
        }
        if is_missing(status) {
            let descriptor = PrivateDescriptor::registry(&self.user)?;
            let attributes = descriptor.attributes();
            // Each newly created key is volatile, with a protected current-user DACL.
            status_ok(unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    self.path.as_ptr(),
                    0,
                    null(),
                    REG_OPTION_VOLATILE,
                    access,
                    &attributes,
                    &mut key,
                    null_mut(),
                )
            })?;
        } else {
            status_ok(status)?;
        }
        let key = RegistryKey(key);
        verify_private_registry(key.0, &self.user)?;
        Ok(Some(key))
    }
}

fn encode(records: &BTreeMap<Uuid, TerminalRecord>, host: Uuid) -> Result<Vec<u8>> {
    ensure!(valid(records), "invalid WezTerm registration records");
    let bytes = serde_json::to_vec(&SavedTerminals {
        schema_version: VERSION,
        host,
        records,
    })?;
    ensure!(
        bytes.len() <= MAX_BYTES as usize,
        "WezTerm registration metadata exceeds limit"
    );
    Ok(bytes)
}

fn decode(bytes: &[u8], host: Uuid) -> Result<BTreeMap<Uuid, TerminalRecord>> {
    let saved: SavedTerminals<BTreeMap<Uuid, TerminalRecord>> =
        serde_json::from_slice(bytes).context("invalid WezTerm registration metadata")?;
    let version_supported = match saved.schema_version {
        1 => saved.records.values().all(|record| record.pane.is_some()),
        VERSION => true,
        _ => false,
    };
    ensure!(
        version_supported && saved.host == host && valid(&saved.records),
        "unsupported WezTerm registration metadata"
    );
    Ok(saved.records)
}

fn valid(records: &BTreeMap<Uuid, TerminalRecord>) -> bool {
    records.len() <= MAX_RECORDS
        && records.iter().all(|(client, record)| {
            !client.is_nil()
                && !record.epoch.is_nil()
                && record.revision != 0
                && record.commit_sequence != 0
                && record.pane.as_ref().is_none_or(WezTermRegistration::valid)
        })
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn is_missing(status: u32) -> bool {
    matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
}

fn status_ok(status: u32) -> Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
            .context("accessing WezTerm registration key")
    }
}

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        // This handle was opened by RegOpenKeyExW or RegCreateKeyExW.
        unsafe { RegCloseKey(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::process::Command;

    use serde_json::json;
    use windows_sys::Win32::System::Registry::RegDeleteKeyW;

    use super::*;

    const CHILD_PATH: &str = "CORVID_TEST_WEZTERM_REGISTRY_PATH";
    const CHILD_IDS: &str = "CORVID_TEST_WEZTERM_REGISTRY_IDS";
    const READ_MARKER: &str = "read the saved WezTerm registration in a new process";

    #[test]
    fn decodes_exact_revisions_and_refuses_another_host_or_newer_schema() {
        let host = Uuid::new_v4();
        let records = fixture_records();
        let bytes = encode(&records, host).unwrap();
        assert_eq!(decode(&bytes, host).unwrap(), records);
        assert!(decode(&bytes, Uuid::new_v4()).is_err());

        let mut newer: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        newer["schema_version"] = json!(VERSION + 1);
        assert!(decode(&serde_json::to_vec(&newer).unwrap(), host).is_err());
    }

    #[test]
    fn legacy_registration_requires_a_pane_and_manual_pairing_is_explicit() {
        let host = Uuid::new_v4();
        let mut records = fixture_records();
        let manual_client = Uuid::from_u128(3);
        let mut version_one: serde_json::Value =
            serde_json::from_slice(&encode(&records, host).unwrap()).unwrap();
        version_one["schema_version"] = json!(1);
        assert!(decode(&serde_json::to_vec(&version_one).unwrap(), host).is_err());
        version_one["records"]
            .as_object_mut()
            .unwrap()
            .remove(&manual_client.to_string());
        records.remove(&manual_client);
        assert_eq!(
            decode(&serde_json::to_vec(&version_one).unwrap(), host).unwrap(),
            records
        );

        version_one["schema_version"] = json!(VERSION);
        let wezterm_client = Uuid::from_u128(2).to_string();
        version_one["records"][wezterm_client]
            .as_object_mut()
            .unwrap()
            .remove("pane");
        assert!(decode(&serde_json::to_vec(&version_one).unwrap(), host).is_err());
    }

    #[test]
    fn restores_only_matching_registry_records_across_processes() {
        if let (Ok(path), Ok(ids)) = (env::var(CHILD_PATH), env::var(CHILD_IDS)) {
            let (logon, ids) = ids.split_once('/').unwrap();
            let (controller, host) = ids.split_once('/').unwrap();
            assert_eq!(current_logon().unwrap(), logon.parse::<u64>().unwrap());
            let registry =
                TerminalRegistry::at(&path, controller.parse().unwrap(), host.parse().unwrap())
                    .unwrap();
            assert_eq!(registry.load().unwrap(), fixture_records());
            println!("{READ_MARKER}");
            return;
        }

        let controller = Uuid::new_v4();
        let host = Uuid::new_v4();
        let path = format!(r"Software\Corvid.WezTerm.Test.{}", Uuid::new_v4());
        let cleanup = TestKey(wide(&path));
        let registry = TerminalRegistry::at(&path, controller, host).unwrap();
        assert!(registry.load().unwrap().is_empty());
        registry.save(&fixture_records()).unwrap();

        let (_, tests) = module_path!().split_once("::").unwrap();
        let child = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{tests}::restores_only_matching_registry_records_across_processes"),
                "--nocapture",
            ])
            .env(CHILD_PATH, &path)
            .env(
                CHILD_IDS,
                format!("{}/{controller}/{host}", current_logon().unwrap()),
            )
            .output()
            .unwrap();
        assert!(
            child.status.success() && String::from_utf8_lossy(&child.stdout).contains(READ_MARKER),
            "registry subprocess output:\n{}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );

        let another_controller = TerminalRegistry::at(&path, Uuid::new_v4(), host).unwrap();
        assert!(another_controller.load().unwrap().is_empty());
        let another_host = TerminalRegistry::at(&path, controller, Uuid::new_v4()).unwrap();
        assert!(another_host.load().is_err());
        registry.save(&BTreeMap::new()).unwrap();
        assert!(registry.load().unwrap().is_empty());

        registry.save(&fixture_records()).unwrap();
        drop(cleanup);
        assert!(registry.load().unwrap().is_empty());
    }

    fn fixture_records() -> BTreeMap<Uuid, TerminalRecord> {
        let record: TerminalRecord = serde_json::from_value(json!({
            "epoch": Uuid::from_u128(1),
            "revision": u64::MAX - 1,
            "commit_sequence": 9_007_199_254_740_993u64,
            "pane": {
                "gui": {
                    "pid": 42,
                    "created": 123,
                    "socket": r"C:\CorvidFixture\gui-sock-42",
                    "file": { "volume": 1, "id": ([1u8; 16]) }
                },
                "location": { "window_id": 0, "tab_id": 0, "pane_id": 0 }
            }
        }))
        .unwrap();
        let manual = TerminalRecord {
            epoch: record.epoch,
            revision: 7,
            commit_sequence: 9_007_199_254_740_993,
            pane: None,
        };
        BTreeMap::from([(Uuid::from_u128(2), record), (Uuid::from_u128(3), manual)])
    }

    struct TestKey(Vec<u16>);

    impl Drop for TestKey {
        fn drop(&mut self) {
            // The path is a NUL-terminated, randomly named key owned by this test.
            unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, self.0.as_ptr()) };
        }
    }
}
