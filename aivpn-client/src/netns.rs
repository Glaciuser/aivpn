#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

#[cfg(target_os = "linux")]
use aivpn_common::error::{Error, Result};

#[cfg(target_os = "linux")]
pub struct NetworkNamespace {
    target_ns: File,
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone)]
pub struct NetworkNamespace;

#[cfg(target_os = "linux")]
impl std::fmt::Debug for NetworkNamespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkNamespace").finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
impl NetworkNamespace {
    pub fn new() -> Result<Self> {
        let original_ns = File::open("/proc/thread-self/ns/net").map_err(Error::Io)?;
        // SAFETY: unshare is called synchronously on the current thread and we
        // restore the original namespace before returning.
        let rc = unsafe { libc::unshare(libc::CLONE_NEWNET) };
        if rc != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }

        let created = (|| -> Result<Self> {
            let target_ns = File::open("/proc/thread-self/ns/net").map_err(Error::Io)?;
            Ok(Self { target_ns })
        })();

        // SAFETY: restore the original namespace on the current thread.
        let restore_rc = unsafe { libc::setns(original_ns.as_raw_fd(), libc::CLONE_NEWNET) };
        if restore_rc != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }

        created
    }

    pub fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Result<T>,
    {
        let original_ns = File::open("/proc/thread-self/ns/net").map_err(Error::Io)?;

        // SAFETY: the calling thread enters the stored namespace only for the
        // duration of this synchronous closure and is restored before return.
        let setns_rc = unsafe { libc::setns(self.target_ns.as_raw_fd(), libc::CLONE_NEWNET) };
        if setns_rc != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }

        let result = f();

        // SAFETY: restore the caller thread back to its original namespace.
        let restore_rc = unsafe { libc::setns(original_ns.as_raw_fd(), libc::CLONE_NEWNET) };
        if restore_rc != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }

        result
    }
}

#[cfg(not(target_os = "linux"))]
impl NetworkNamespace {
    pub fn new() -> aivpn_common::error::Result<Self> {
        Err(aivpn_common::error::Error::Session(
            "SOCKS5 mode requires Linux network namespace support".into(),
        ))
    }

    pub fn run<F, T>(&self, _f: F) -> aivpn_common::error::Result<T>
    where
        F: FnOnce() -> aivpn_common::error::Result<T>,
    {
        Err(aivpn_common::error::Error::Session(
            "SOCKS5 mode requires Linux network namespace support".into(),
        ))
    }
}
