use std::ffi::c_void;
use std::io;
use std::mem::offset_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null_mut;

use anyhow::{Context, Result, ensure};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce,
    GetSecurityDescriptorControl, GetTokenInformation, IsValidSid, OWNER_SECURITY_INFORMATION,
    PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::core::BOOL;

const SID_LIMIT: usize = 256;

pub(super) fn current_user() -> Result<String> {
    let mut token: HANDLE = null_mut();
    // OpenProcessToken writes a new handle to the supplied pointer.
    checked(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) })
        .context("opening current user token")?;
    // The successful call transfers ownership of the handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0;
    // A zero-sized call reports the required storage size.
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size) };
    ensure!(
        (size_of::<TOKEN_USER>()..=4096).contains(&(size as usize)),
        "invalid Windows user token size"
    );
    let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    // The aligned buffer has at least the requested size.
    checked(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    })?;
    // TokenUser supplies a TOKEN_USER and a SID in the live allocation.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    sid_text(user.User.Sid)
}

pub(super) struct PrivateDescriptor(LocalAllocation);

impl PrivateDescriptor {
    pub(super) fn new(user: &str, directory: bool) -> Result<Self> {
        let inheritance = if directory { "OICI" } else { "" };
        let sddl = format!("O:{user}D:P(A;{inheritance};FA;;;{user})");
        let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = null_mut();
        // The input is NUL-terminated and Windows allocates the result with LocalAlloc.
        checked(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        })
        .context("creating private Windows security descriptor")?;
        Ok(Self(LocalAllocation(descriptor)))
    }

    pub(super) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: 0,
        }
    }
}

pub(super) fn verify_private(handle: HANDLE, user: &str, require_protected: bool) -> Result<()> {
    let mut owner = null_mut();
    let mut acl: *mut ACL = null_mut();
    let mut descriptor = null_mut();
    // Windows writes the owner and ACL pointers into the allocated descriptor.
    let result = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(result as i32))
            .context("checking Windows object ACL");
    }
    let descriptor = LocalAllocation(descriptor);
    ensure!(
        sid_text(owner)? == user,
        "Windows object belongs to a different user"
    );
    ensure!(!acl.is_null(), "Windows object has no private ACL");
    // The ACL is non-null and owned by the descriptor until this function returns.
    ensure!(
        unsafe { (*acl).AceCount } == 1,
        "Windows object ACL grants additional access"
    );
    verify_user_ace(acl, user)?;
    if require_protected {
        let mut control = 0;
        let mut revision = 0;
        // The descriptor remains allocated and both result pointers are writable.
        checked(unsafe {
            GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision)
        })?;
        ensure!(
            control & SE_DACL_PROTECTED != 0,
            "Windows state ACL must block parent inheritance"
        );
    }
    Ok(())
}

fn verify_user_ace(acl: *const ACL, user: &str) -> Result<()> {
    let mut ace: *mut c_void = null_mut();
    // The caller verified there is one ACE in the live ACL.
    checked(unsafe { GetAce(acl, 0, &mut ace) })?;
    ensure!(!ace.is_null(), "Windows object ACL entry is missing");
    // Every ACE returned by GetAce starts with a valid ACE_HEADER.
    let header = unsafe { &*ace.cast::<ACE_HEADER>() };
    ensure!(
        u32::from(header.AceType) == ACCESS_ALLOWED_ACE_TYPE
            && usize::from(header.AceSize) >= size_of::<ACCESS_ALLOWED_ACE>() + 4,
        "invalid private Windows access rule"
    );
    // The type and size check establish the ACE layout and beginning of its SID.
    let access = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    let sid: PSID = std::ptr::addr_of!(access.SidStart).cast_mut().cast();
    // The size check above includes the SID header and its subauthority count.
    let count = unsafe { *sid.cast::<u8>().add(1) };
    ensure!(
        usize::from(header.AceSize)
            >= offset_of!(ACCESS_ALLOWED_ACE, SidStart) + 8 + 4 * usize::from(count),
        "invalid SID length in Windows access rule"
    );
    ensure!(
        access.Mask == FILE_ALL_ACCESS && sid_text(sid)? == user,
        "Windows object ACL must grant access only to the current user"
    );
    Ok(())
}

fn sid_text(sid: PSID) -> Result<String> {
    // SIDs passed here originate in an OS token or security descriptor.
    ensure!(
        !sid.is_null() && unsafe { IsValidSid(sid) } != 0,
        "invalid Windows SID"
    );
    let mut text = null_mut();
    // Windows writes a pointer to a LocalAlloc-backed NUL-terminated string.
    checked(unsafe { ConvertSidToStringSidW(sid, &mut text) })?;
    let _allocation = LocalAllocation(text.cast());
    // The Windows API guarantees the returned string has a NUL terminator.
    let len = (0..SID_LIMIT)
        .find(|&index| unsafe { *text.add(index) == 0 })
        .context("Windows SID exceeds size bound")?;
    let slice = unsafe { std::slice::from_raw_parts(text, len) };
    Ok(String::from_utf16(slice)?)
}

pub(super) fn checked(result: BOOL) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // All pointers wrapped here were allocated by documented LocalAlloc APIs.
            unsafe { LocalFree(self.0) };
        }
    }
}
