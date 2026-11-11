//! Windows per-user SID + current-user-only pipe ACL helpers (Lattice §5).
//!
//! The NAF pipe name and its DACL must be derived from the actual user SID, not
//! from `USERNAME` (spoofable/derived text). The authoritative SID is read from
//! the owning process token.

use std::ptr;

#[cfg(windows)]
use windows::core::PWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
#[cfg(windows)]
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_ACCESS_MASK, TOKEN_USER,
    TokenUser,
};
#[cfg(windows)]
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const TOKEN_QUERY: TOKEN_ACCESS_MASK = TOKEN_ACCESS_MASK(0x8);

/// The current process token's user SID as a string (e.g. `S-1-5-21-…-1001`).
#[cfg(windows)]
pub(crate) fn current_user_sid_string() -> Option<String> {
    unsafe {
        let mut token: HANDLE = HANDLE(std::ptr::null_mut());
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return None;
        }
        if token.is_invalid() {
            return None;
        }
        let _guard = ProcessTokenGuard(token);

        let mut buf: Vec<u8> = vec![0u8; 256];
        let mut needed: u32 = 0;
        if GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            buf.len() as u32,
            &mut needed,
        )
        .is_err()
        {
            let size = needed as usize;
            if size > buf.len() {
                buf.resize(size, 0u8);
            }
            if GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                buf.len() as u32,
                &mut needed,
            )
            .is_err()
            {
                return None;
            }
        }
        let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
        let sid = token_user.User.Sid;
        let mut out: PWSTR = PWSTR(ptr::null_mut());
        if ConvertSidToStringSidW(sid, &mut out).is_err() || out.0.is_null() {
            return None;
        }
        let s = out.to_string().ok()?;
        LocalFree(windows::Win32::Foundation::HLOCAL(out.0 as *mut _));
        Some(s)
    }
}

/// Drop guard that closes the open process token handle.
#[cfg(windows)]
struct ProcessTokenGuard(HANDLE);

#[cfg(windows)]
impl Drop for ProcessTokenGuard {
    fn drop(&mut self) {
        unsafe { let _ = CloseHandle(self.0); }
    }
}

/// Fallback used only on non-Windows (keeps the crate cross-platform).
#[cfg(not(windows))]
pub(crate) fn current_user_sid_string() -> Option<String> {
    None
}

/// Per-user singleton guard (GATE 5): a named mutex whose name is derived from
/// the authoritative user SID. If another host already holds it, this process
/// exits immediately — exactly one persistent NAI-Lattice singleton. Uses native
/// Windows process identity; stale owners (crashed) are auto-released by the OS.
#[cfg(windows)]
pub(crate) fn acquire_singleton() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let sid = current_user_sid_string()?;
    let name = format!("Local\\NAI.AmbientFabric.Singleton.{sid}");
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let result = CreateMutexW(None, false, windows::core::PCWSTR(wide.as_ptr()));
        let mutex = match result {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "CreateMutexW failed");
                return None;
            }
        };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return None;
        }
        Some(mutex)
    }
}

/// Drop guard for the singleton mutex handle.
#[cfg(windows)]
pub(crate) struct SingletonGuard(pub(crate) windows::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Drop for SingletonGuard {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        unsafe { let _ = CloseHandle(self.0); }
    }
}

/// A security descriptor granting GENERIC_ALL access ONLY to `sid_string`
/// (current user), used as the pipe's `lpSecurityAttributes`.
/// Returns the raw descriptor pointer; caller frees with `free_security_descriptor`.
#[cfg(windows)]
pub(crate) fn current_user_only_descriptor(sid_string: &str) -> Option<*mut core::ffi::c_void> {
    let dacl = format!("D:P(A;;GA;;;{sid_string})");
    let wide: Vec<u16> = dacl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = PSECURITY_DESCRIPTOR::default();
    let mut size: u32 = 0;
    unsafe {
        let ok = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            windows::core::PCWSTR(wide.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            Some(&mut size),
        );
        if ok.is_err() || sd.0.is_null() {
            return None;
        }
    }
    Some(sd.0)
}

/// Wrap a descriptor in a `SECURITY_ATTRIBUTES` for passing to `CreateNamedPipeW`.
#[cfg(windows)]
pub(crate) fn security_attributes_for(sd: *mut core::ffi::c_void) -> Option<SECURITY_ATTRIBUTES> {
    if sd.is_null() {
        return None;
    }
    Some(SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd as *mut _,
        bInheritHandle: windows::Win32::Foundation::BOOL(0),
    })
}

/// Free a descriptor created by `current_user_only_descriptor`.
#[cfg(windows)]
pub(crate) unsafe fn free_security_descriptor(sd: *mut core::ffi::c_void) {
    if !sd.is_null() {
        LocalFree(windows::Win32::Foundation::HLOCAL(sd));
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use windows::core::PWSTR;
    #[allow(unused_imports)]
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;
    #[allow(unused_imports)]
    use windows::Win32::Security::Authorization::SDDL_REVISION_1;

    #[test]
    fn sid_derived_pipe_is_sid_not_username() {
        let sid = crate::win_acl::current_user_sid_string();
        if let Some(s) = sid {
            assert!(s.starts_with("S-1-"), "expected a SID, got {s}");
            assert!(s.len() > 8, "SID too short: {s}");
            assert!(!s.to_lowercase().starts_with("user"), "must be SID, not username: {s}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn current_user_only_descriptor_round_trips() {
        use windows::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
        let sid = crate::win_acl::current_user_sid_string().expect("sid");
        let sd = crate::win_acl::current_user_only_descriptor(&sid).expect("descriptor");
        unsafe {
            let p = PSECURITY_DESCRIPTOR(sd);
            let mut out: PWSTR = PWSTR(std::ptr::null_mut());
            let mut len: u32 = 0;
            let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
                p,
                SDDL_REVISION_1,
                windows::Win32::Security::DACL_SECURITY_INFORMATION
                    | windows::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION,
                &mut out,
                Some(&mut len),
            );
            let text = if ok.is_err() {
                String::new()
            } else {
                out.to_string().unwrap_or_default()
            };
            crate::win_acl::free_security_descriptor(sd);
            assert!(!text.is_empty(), "failed to stringify descriptor");
            assert!(text.contains(&sid), "descriptor lacks owner SID grant: {text}");
        }
    }
}