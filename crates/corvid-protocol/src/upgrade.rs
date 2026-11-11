use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Encode a path for the authenticated local upgrade connection.
pub fn encode_state_path(path: &Path) -> Vec<u8> {
    #[cfg(target_os = "linux")]
    {
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
}

/// Decode the host's native path without requiring valid Unicode.
pub fn decode_state_path(bytes: Vec<u8>) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let path = {
        if bytes.contains(&0) {
            return None;
        }
        PathBuf::from(OsString::from_vec(bytes))
    };
    #[cfg(windows)]
    let path = {
        let pairs = bytes.chunks_exact(2);
        if !pairs.remainder().is_empty() {
            return None;
        }
        let units: Vec<_> = pairs
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        if units.contains(&0) {
            return None;
        }
        PathBuf::from(OsString::from_wide(&units))
    };
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_state_path_round_trips_native_characters() {
        #[cfg(target_os = "linux")]
        let path = PathBuf::from(OsString::from_vec(b"/state/\xff".to_vec()));
        #[cfg(windows)]
        let path = {
            let units: Vec<_> = "C:\\state\\".encode_utf16().chain([0xd800]).collect();
            PathBuf::from(OsString::from_wide(&units))
        };
        assert_eq!(decode_state_path(encode_state_path(&path)), Some(path));
        assert!(decode_state_path(encode_state_path(Path::new("relative"))).is_none());
    }
}
