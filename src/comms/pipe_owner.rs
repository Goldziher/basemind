//! Opening the daemon's named pipe only when this user's process serves it.
//!
//! The pipe name is derived from the user name and the comms directory, so another local user who
//! knows both can create the first instance before the daemon does. A client that connected to it
//! would hand that user its `Hello`, its paths and its memory writes. So every client checks, once
//! connected, that the process serving the pipe runs as the same user (its token's SID) and refuses
//! the pipe otherwise.

#![cfg(windows)]

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

/// `OpenProcess` access right to read a process's token.
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
/// `OpenProcessToken` access right to read a token.
const TOKEN_QUERY: u32 = 0x0008;
/// `TOKEN_INFORMATION_CLASS::TokenUser`.
const TOKEN_USER: u32 = 1;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetNamedPipeServerProcessId(pipe: isize, server_pid: *mut u32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn GetCurrentProcess() -> *mut c_void;
    fn CloseHandle(handle: isize) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: isize, access: u32, token: *mut isize) -> i32;
    fn GetTokenInformation(token: isize, class: u32, info: *mut c_void, len: u32, returned: *mut u32) -> i32;
    fn EqualSid(a: *mut c_void, b: *mut c_void) -> i32;
}

/// Open `path` like `ClientOptions::new().open`, then refuse it with `PermissionDenied` unless the
/// process serving it runs as this process's user.
pub fn open_verified(path: &Path) -> std::io::Result<NamedPipeClient> {
    let client = ClientOptions::new().open(path)?;
    if served_by_this_user(&client)? {
        Ok(client)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "the pipe {} is served by a process of another user; refusing to talk to it",
                path.display()
            ),
        ))
    }
}

/// Whether the process at the server end of `client` runs as this process's user.
fn served_by_this_user(client: &NamedPipeClient) -> std::io::Result<bool> {
    let mut server_pid = 0u32;
    // SAFETY: the handle is the live client end of a pipe; `server_pid` is a valid out-pointer.
    if unsafe { GetNamedPipeServerProcessId(client.as_raw_handle() as isize, &mut server_pid) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `OpenProcess` takes plain values; a zero handle is the failure checked below.
    let server = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, server_pid) };
    if server == 0 {
        // A process of this user can always be opened for this right; one that cannot is not ours.
        return Ok(false);
    }
    let theirs = token_user(server);
    // SAFETY: `server` is a handle this function opened and closes once.
    unsafe { CloseHandle(server) };
    // SAFETY: the pseudo-handle of the current process needs no closing.
    let ours = token_user(unsafe { GetCurrentProcess() } as isize)?;
    let Ok(theirs) = theirs else {
        return Ok(false);
    };
    // SAFETY: each buffer starts with a `TOKEN_USER` whose first field points at a SID inside the
    // same buffer, which lives until the end of this function.
    Ok(unsafe { EqualSid(sid_of(&theirs), sid_of(&ours)) } != 0)
}

/// The `TOKEN_USER` of `process`, in a buffer aligned for the pointer it starts with.
fn token_user(process: isize) -> std::io::Result<Vec<u64>> {
    let mut token = 0isize;
    // SAFETY: `token` is a valid out-pointer; the handle is closed below.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut needed = 0u32;
    // SAFETY: a null buffer of length 0 asks only for the size, written to `needed`.
    unsafe { GetTokenInformation(token, TOKEN_USER, std::ptr::null_mut(), 0, &mut needed) };
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
    let len = u32::try_from(buffer.len() * 8).unwrap_or(u32::MAX);
    // SAFETY: `buffer` holds `len` writable bytes.
    let filled = unsafe { GetTokenInformation(token, TOKEN_USER, buffer.as_mut_ptr().cast(), len, &mut needed) };
    let error = std::io::Error::last_os_error();
    // SAFETY: `token` was opened above and is closed once.
    unsafe { CloseHandle(token) };
    if filled == 0 { Err(error) } else { Ok(buffer) }
}

/// The SID a `TOKEN_USER` buffer points at.
///
/// # Safety
/// `buffer` must hold a `TOKEN_USER` filled by `GetTokenInformation`.
unsafe fn sid_of(buffer: &[u64]) -> *mut c_void {
    // SAFETY: per the contract, the buffer starts with the SID pointer of a `SID_AND_ATTRIBUTES`.
    unsafe { *buffer.as_ptr().cast::<*mut c_void>() }
}

#[cfg(test)]
mod tests {
    use tokio::net::windows::named_pipe::ServerOptions;

    use super::*;

    /// A pipe this process serves is served by this user, and is opened.
    #[tokio::test]
    async fn a_pipe_served_by_this_user_is_opened() {
        let name = format!(r"\\.\pipe\basemind-owner-test-{}", std::process::id());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .expect("create");
        let connecting = server.connect();
        let client = open_verified(Path::new(&name)).expect("same user");
        connecting.await.expect("connected");
        drop(client);
    }
}
