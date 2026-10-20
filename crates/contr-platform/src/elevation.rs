//! Programs run with administrator rights after the user approves the Windows elevation prompt.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, WAIT_FAILED};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Runs `program` with `parameters` elevated and waits for its exit code; blocks while the prompt and the program run.
/// A declined prompt fails with [`io::ErrorKind::PermissionDenied`].
#[allow(unsafe_code)]
pub fn run_elevated(program: &str, parameters: &str) -> io::Result<u32> {
    let verb = wide("runas");
    let file = wide(program);
    let parameters = wide(parameters);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>()).unwrap_or(u32::MAX),
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };

    // SAFETY: `info` is fully initialized, its strings are NUL-terminated and outlive the call.
    if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == i32::try_from(ERROR_CANCELLED).ok() {
            io::Error::new(io::ErrorKind::PermissionDenied, "the administrator prompt was declined")
        } else {
            error
        });
    }

    let process = info.hProcess;
    // SAFETY: `process` is the handle SEE_MASK_NOCLOSEPROCESS returned; it is closed exactly once below.
    let waited = unsafe { WaitForSingleObject(process, INFINITE) };
    let mut code = 0u32;
    // SAFETY: as above; `code` is a valid output location.
    let read = unsafe { GetExitCodeProcess(process, &raw mut code) };
    let read_error = (read == 0).then(io::Error::last_os_error);
    // SAFETY: as above.
    unsafe { CloseHandle(process) };

    if waited == WAIT_FAILED {
        return Err(io::Error::other("cannot wait for the elevated program"));
    }
    read_error.map_or(Ok(code), Err)
}

fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain([0]).collect()
}
