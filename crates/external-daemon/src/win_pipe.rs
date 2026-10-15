//! Shared Win32 named-pipe primitives (Windows only).
//!
//! Both the RPC transport (`rpc/windows.rs`) and the daemon MCP listener
//! (`daemon_windows.rs`) create their endpoints here so the anti-hijack flag
//! (`FILE_FLAG_FIRST_PIPE_INSTANCE`) and the current-user-only DACL are built
//! once, from a single SDDL construction.

use std::{io, path::Path, ptr};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_ACCESS_DENIED, HANDLE, INVALID_HANDLE_VALUE,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    },
    Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
    },
    System::{
        Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

/// Pipe buffer size for each direction. Large enough that a control frame is
/// buffered without a partial-write stall in normal use.
pub(crate) const PIPE_BUFFER_BYTES: u32 = 64 * 1024;

/// Owns a security descriptor and the `SECURITY_ATTRIBUTES` that reference it
/// for the lifetime of the listener. The DACL grants full control to the
/// current user SID and LocalSystem; every other principal is implicitly denied.
pub(crate) struct SecurityAttributes {
    attrs: SECURITY_ATTRIBUTES,
    descriptor: PSECURITY_DESCRIPTOR,
}

unsafe impl Send for SecurityAttributes {}

impl SecurityAttributes {
    /// Returns the attributes. The method receiver forces a whole-struct
    /// closure capture so the explicit `Send` impl above governs the moved
    /// value rather than the raw `*mut c_void` field alone.
    pub(crate) fn attributes(&self) -> &SECURITY_ATTRIBUTES {
        &self.attrs
    }

    pub(crate) fn current_user() -> io::Result<Self> {
        let mut token: HANDLE = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = Self::from_token(token);
        unsafe { CloseHandle(token) };
        result
    }

    fn from_token(token: HANDLE) -> io::Result<Self> {
        let mut length = 0u32;
        unsafe {
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut length);
        }
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; length as usize];
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sid = unsafe { (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid };
        let mut sid_string: *mut u16 = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut sid_string) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = unsafe {
            let mut len = 0usize;
            while *sid_string.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(sid_string, len))
        };
        unsafe { LocalFree(sid_string as _) };
        // `D:P` marks the DACL as protected (no inherited ACEs); only the
        // current user and LocalSystem receive access.
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)")
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            attrs: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
            descriptor,
        })
    }
}

impl Drop for SecurityAttributes {
    fn drop(&mut self) {
        unsafe { LocalFree(self.descriptor) };
    }
}

/// Encode a pipe path as a NUL-terminated wide string.
pub(crate) fn wide(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let name: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    if name.len() <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "named pipe path is empty",
        ));
    }
    Ok(name)
}

/// Create one overlapped, byte-mode duplex pipe instance.
///
/// `first` adds `FILE_FLAG_FIRST_PIPE_INSTANCE`; a pre-existing pipe with the
/// same name then fails with `ERROR_ACCESS_DENIED`, which is surfaced as
/// `AddrInUse` so callers fail loudly instead of silently degrading.
pub(crate) fn create_instance(
    name: &[u16],
    max_instances: u32,
    first: bool,
    attributes: &SECURITY_ATTRIBUTES,
) -> io::Result<HANDLE> {
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            max_instances,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            attributes as *const _,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = unsafe { GetLastError() };
        if error == ERROR_ACCESS_DENIED {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "named pipe endpoint already exists",
            ));
        }
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    Ok(handle)
}
