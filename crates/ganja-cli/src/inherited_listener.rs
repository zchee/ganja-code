//! The worker's one explicit inherited descriptor, validated before ownership.
//!
//! The handoff passes one descriptor argument and leaves the environment
//! allowlist unchanged. `listenfd`'s systemd environment protocol is not that
//! contract and would not remove the separate raw-descriptor, address and
//! listening-state validation required here. Existing libc handles this boundary;
//! owned descriptor flag operations use rustix.

use std::mem::{self, MaybeUninit};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};

use anyhow::{Context as _, Result, bail, ensure};
use tokio::net::TcpListener;

/// Takes the unowned listener inherited by this fresh serve process.
/// Returns a Tokio listener after validating `number` against `expected`.
/// Refused numbers are never turned into handles and are never closed.
///
/// # Errors
///
/// Refuses invalid descriptors, non-listening TCP sockets, address mismatches,
/// failed kernel probes or flag updates, and failed Tokio registration.
pub(crate) fn take(number: RawFd, expected: SocketAddr) -> Result<TcpListener> {
    ensure!(number >= 3, "the inherited listener descriptor must be at least 3");
    // SAFETY: raw fallible queries are required before any handle exists;
    // neither this probe nor any refusal transfers or closes the descriptor.
    let flags = unsafe { libc::fcntl(number, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error())
            .context("inherited listener descriptor is not open");
    }
    ensure!(
        socket_option(number, libc::SO_TYPE)? == libc::SOCK_STREAM,
        "inherited listener is not a stream socket"
    );
    let address = local_address(number)?;
    ensure!(is_listening(number)?, "inherited stream socket is not listening");
    ensure!(address == expected, "inherited listener address {address} does not match {expected}");
    // SAFETY: the descriptor passed every socket probe; F_SETFD changes only
    // descriptor flags and preserves every flag returned by the checked get.
    if unsafe { libc::fcntl(number, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error()).context("failed to reseal inherited listener");
    }
    // SAFETY: number passed the open, listening TCP and address probes. Nothing
    // in this fresh process creates a listening TCP socket before this point,
    // so it is the descriptor inherited across exec and nothing else owns it.
    // Adopt it exactly once; this owner closes it on every later error.
    let owned = unsafe { OwnedFd::from_raw_fd(number) };
    let listener = std::net::TcpListener::from(owned);
    listener.set_nonblocking(true).context("failed to make inherited listener nonblocking")?;
    TcpListener::from_std(listener).context("failed to register inherited listener")
}

#[cfg(not(target_vendor = "apple"))]
fn is_listening(number: RawFd) -> Result<bool> {
    Ok(socket_option(number, libc::SO_ACCEPTCONN)? == 1)
}

#[cfg(target_vendor = "apple")]
fn is_listening(number: RawFd) -> Result<bool> {
    // Darwin defines SO_ACCEPTCONN but does not expose it through getsockopt.
    // TCP_CONNECTION_INFO exposes the same state; tcp_fsm.h defines LISTEN as 1.
    const TCP_LISTEN: u8 = 1;
    let mut info = MaybeUninit::<libc::tcp_connection_info>::zeroed();
    let mut length = mem::size_of::<libc::tcp_connection_info>() as libc::socklen_t;
    // SAFETY: info is zero-initialized, aligned and writable for length bytes;
    // this raw fallible query neither borrows nor takes ownership of number.
    if unsafe {
        libc::getsockopt(
            number,
            libc::IPPROTO_TCP,
            libc::TCP_CONNECTION_INFO,
            info.as_mut_ptr().cast(),
            &raw mut length,
        )
    } == -1
    {
        return Err(std::io::Error::last_os_error()).context("failed to probe inherited TCP state");
    }
    // Kernel and libc versions may expose different tails; only the leading
    // state byte is needed, and the entire supplied buffer began initialized.
    ensure!(
        (1..=mem::size_of::<libc::tcp_connection_info>()).contains(&(length as usize)),
        "invalid inherited TCP state size"
    );
    // SAFETY: the whole buffer began initialized and the size-checked query succeeded.
    Ok(unsafe { info.assume_init() }.tcpi_state == TCP_LISTEN)
}

fn socket_option(number: RawFd, option: libc::c_int) -> Result<libc::c_int> {
    let mut value = 0;
    let mut length = mem::size_of_val(&value) as libc::socklen_t;
    // SAFETY: value is writable for length bytes; the kernel validates number
    // without requiring us to invent a borrowed or owning handle for it.
    if unsafe {
        libc::getsockopt(number, libc::SOL_SOCKET, option, (&raw mut value).cast(), &raw mut length)
    } == -1
    {
        return Err(std::io::Error::last_os_error())
            .context("inherited descriptor is not a usable socket");
    }
    ensure!(length as usize == mem::size_of_val(&value), "invalid inherited socket option size");
    Ok(value)
}

fn local_address(number: RawFd) -> Result<SocketAddr> {
    let mut storage = MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut length = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: storage is large and aligned for either internet address and
    // zero-initialized; getsockname validates number and reports written size.
    if unsafe { libc::getsockname(number, storage.as_mut_ptr().cast(), &raw mut length) } == -1 {
        return Err(std::io::Error::last_os_error())
            .context("failed to probe inherited listener address");
    }
    // SAFETY: the storage began fully initialized and getsockname succeeded.
    let storage = unsafe { storage.assume_init() };
    match libc::c_int::from(storage.ss_family) {
        libc::AF_INET if length as usize >= mem::size_of::<libc::sockaddr_in>() => {
            // SAFETY: family and reported size establish an initialized IPv4
            // address in storage, whose alignment is sufficient for this type.
            let address = unsafe { (&raw const storage).cast::<libc::sockaddr_in>().read() };
            let ip = Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes());
            Ok(SocketAddr::new(IpAddr::V4(ip), u16::from_be(address.sin_port)))
        }
        libc::AF_INET6 if length as usize >= mem::size_of::<libc::sockaddr_in6>() => {
            // SAFETY: family and reported size establish an initialized IPv6
            // address in storage, whose alignment is sufficient for this type.
            let address = unsafe { (&raw const storage).cast::<libc::sockaddr_in6>().read() };
            let ip = Ipv6Addr::from(address.sin6_addr.s6_addr);
            Ok(SocketAddr::new(IpAddr::V6(ip), u16::from_be(address.sin6_port)))
        }
        _ => bail!("inherited listener is not an IPv4 or IPv6 socket"),
    }
}

#[cfg(test)]
#[path = "inherited_listener_tests.rs"]
mod tests;
