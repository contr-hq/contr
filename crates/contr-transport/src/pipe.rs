#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::pin::Pin;
use std::ptr;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};
use tonic::transport::server::Connected;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_BUSY, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::{Endpoint, TransportError};

const BUSY_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Accepted pipe connection, ready for `tonic::transport::Server::serve_with_incoming`.
pub struct Connection(NamedPipeServer);

impl Connected for Connection {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for Connection {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Connection {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Named-pipe listener whose instances only the current user and SYSTEM may open, and only from this machine.
pub struct Listener {
    name: String,
    security: SecurityDescriptor,
    next: NamedPipeServer,
}

impl Listener {
    /// Creates the first instance of `endpoint`; fails when another process already owns the pipe name.
    pub fn bind(endpoint: &Endpoint) -> Result<Self, TransportError> {
        let Endpoint::NamedPipe(name) = endpoint else {
            return Err(TransportError::Unsupported("Unix socket"));
        };
        let sddl = format!("D:P(A;;GA;;;{})(A;;GA;;;SY)", current_user_sid()?);
        let security = SecurityDescriptor::from_sddl(&sddl)?;
        let next = security.create_instance(name, true)?;

        Ok(Self {
            name: name.clone(),
            security,
            next,
        })
    }

    /// Endpoint clients connect to.
    pub fn endpoint(&self) -> Option<Endpoint> {
        Some(Endpoint::NamedPipe(self.name.clone()))
    }

    /// Connected pipe instances; a new instance is created before each connection is handed out.
    pub fn incoming(self) -> Pin<Box<dyn Stream<Item = io::Result<Connection>> + Send>> {
        Box::pin(futures_util::stream::unfold(self, |mut listener| async move {
            let accepted = listener.accept().await;
            Some((accepted, listener))
        }))
    }

    async fn accept(&mut self) -> io::Result<Connection> {
        self.next.connect().await?;
        let next = self.security.create_instance(&self.name, false)?;
        Ok(Connection(std::mem::replace(&mut self.next, next)))
    }
}

pub(crate) async fn open_client(name: &str) -> io::Result<NamedPipeClient> {
    loop {
        match ClientOptions::new().open(name) {
            Err(error) if error.raw_os_error() == i32::try_from(ERROR_PIPE_BUSY).ok() => {
                tokio::time::sleep(BUSY_RETRY_DELAY).await;
            }
            result => return result,
        }
    }
}

/// String form (`S-1-5-21-…`) of the SID the current process runs as.
pub fn current_user_sid() -> io::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing; `token` is a valid out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);

    let mut length = 0u32;
    // SAFETY: a null buffer with length 0 only queries the required length into `length`.
    unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &raw mut length) };
    let words = (length as usize).div_ceil(size_of::<u64>());
    let mut buffer = vec![0u64; words];
    // SAFETY: `buffer` holds at least `length` bytes and u64 alignment satisfies TOKEN_USER.
    if unsafe { GetTokenInformation(token.0, TokenUser, buffer.as_mut_ptr().cast(), length, &raw mut length) } == 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: GetTokenInformation filled `buffer` with a TOKEN_USER whose SID points into the same buffer.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is valid while `buffer` lives; on success `text` receives a LocalAlloc'd string freed below.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut length = 0;
    // SAFETY: `text` is NUL-terminated, so every unit up to and including the NUL is readable.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: the first `length` units of `text` are initialized.
    let units = unsafe { std::slice::from_raw_parts(text, length) };
    let sid = String::from_utf16_lossy(units);
    // SAFETY: `text` came from LocalAlloc and is not used after this call.
    unsafe { LocalFree(text.cast()) };

    Ok(sid)
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcessToken and is closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is an immutable LocalAlloc'd block owned by this value and freed only in Drop.
unsafe impl Send for SecurityDescriptor {}

// SAFETY: the descriptor is never written after creation, so shared references cannot race.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated and `descriptor` is a valid out pointer; the result is freed in Drop.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self(descriptor))
    }

    fn create_instance(&self, name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        };
        let mut options = ServerOptions::new();
        options.first_pipe_instance(first).reject_remote_clients(true);

        // SAFETY: `attributes` and the descriptor it points to outlive the call, which copies both.
        unsafe { options.create_with_security_attributes_raw(name, (&raw mut attributes).cast::<c_void>()) }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor came from LocalAlloc inside ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;

    #[test]
    fn reads_the_current_user_sid() {
        assert!(current_user_sid().unwrap().starts_with("S-1-"));
    }

    #[tokio::test]
    async fn serves_the_owner_and_refuses_a_second_listener() {
        let name = format!(r"\\.\pipe\contr-test-{}", std::process::id());
        let endpoint = Endpoint::NamedPipe(name.clone());
        let mut incoming = Listener::bind(&endpoint).unwrap().incoming();

        let _client = open_client(&name).await.unwrap();

        assert!(incoming.next().await.unwrap().is_ok());
        assert!(Listener::bind(&endpoint).is_err());
    }
}
