use std::net::{SocketAddr, TcpListener};
use std::os::fd::{AsRawFd as _, RawFd};
use std::path::Path;
use std::process::Stdio;

/// Runs the real serve binary with only the selected socket inherited.
/// The caller keeps `listener` alive through spawn, then drops its own handle.
pub fn command(listener: &TcpListener, home: &Path, project: &Path) -> tokio::process::Command {
    let address = listener.local_addr().expect("inherited address");
    let number = listener.as_raw_fd();
    let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin!("ganja"));
    command
        .args([
            "serve",
            "--hostname",
            &address.ip().to_string(),
            "--port",
            &address.port().to_string(),
            "--listen-fd",
            &number.to_string(),
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("PATH"))
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("GANJA_CONFIG_HOME", home.join("config"))
        .env("GANJA_PROVIDER", "fake")
        .env("GANJA_DISABLE_MODELS_FETCH", "1")
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(home.join("stdout")).expect("stdout"))
        .stderr(std::fs::File::create(home.join("stderr")).expect("stderr"))
        .kill_on_drop(true);
    inherit(&mut command, listener);
    command
}

/// The caller must retain `descriptor` until spawn has completed.
pub fn inherit(command: &mut tokio::process::Command, descriptor: &impl std::os::fd::AsRawFd) {
    let number = descriptor.as_raw_fd();
    // SAFETY: the caller retains the live descriptor through spawn. The child
    // changes only this descriptor's flags using async-signal-safe syscalls;
    // no borrowed or owning raw-descriptor handle is manufactured in pre_exec.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(number, libc::F_GETFD);
            if flags == -1 || libc::fcntl(number, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// A read-only descriptor probe: no ownership or close, even for an invalid fd.
pub fn matches(number: RawFd, expected: SocketAddr) -> std::io::Result<bool> {
    let mut storage = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut length = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: the buffer is aligned, initialized and writable for length bytes;
    // getsockname is fallible for closed and non-socket descriptor numbers.
    if unsafe { libc::getsockname(number, storage.as_mut_ptr().cast(), &raw mut length) } == -1 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::EBADF | libc::ENOTSOCK) => Ok(false),
            Some(libc::EOPNOTSUPP) => {
                // A protocol without a local socket name cannot be the announced TCP listener.
                // System libraries in the test process can pass such sockets to every child.
                Ok(false)
            }
            _ => Err(socket_name_error(number, error)),
        };
    }
    // SAFETY: all buffer bytes were initialized before the successful query.
    let storage = unsafe { storage.assume_init() };
    let same_address = match libc::c_int::from(storage.ss_family) {
        libc::AF_INET if length as usize >= std::mem::size_of::<libc::sockaddr_in>() => {
            // SAFETY: family, size and alignment establish the address layout.
            let address = unsafe { (&raw const storage).cast::<libc::sockaddr_in>().read() };
            let ip = std::net::Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes());
            SocketAddr::new(ip.into(), u16::from_be(address.sin_port)) == expected
        }
        libc::AF_INET6 if length as usize >= std::mem::size_of::<libc::sockaddr_in6>() => {
            // SAFETY: family, size and alignment establish the address layout.
            let address = unsafe { (&raw const storage).cast::<libc::sockaddr_in6>().read() };
            let ip = std::net::Ipv6Addr::from(address.sin6_addr.s6_addr);
            SocketAddr::new(ip.into(), u16::from_be(address.sin6_port)) == expected
        }
        _ => false,
    };
    if !same_address
        || integer_option(number, libc::SOL_SOCKET, libc::SO_TYPE, "getsockopt(SO_TYPE)")?
            != libc::SOCK_STREAM
    {
        return Ok(false);
    }
    listening(number)
}

/// Bounds the test-only scan; system libraries may pass extra descriptors to children.
pub fn scan_bound() -> std::io::Result<i32> {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: the initialized limit is writable and getrlimit changes no process state.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } == -1 {
        return Err(query_error("getrlimit", -1, std::io::Error::last_os_error()));
    }
    i32::try_from(limit.rlim_cur.min(4096)).map_err(std::io::Error::other)
}

/// Finds a matching listener within the scan bound; unexpected syscall errors propagate.
pub fn scan(expected: SocketAddr) -> std::io::Result<bool> {
    for number in 3..scan_bound()? {
        // SAFETY: F_GETFD is a raw read-only query and creates no handle.
        if unsafe { libc::fcntl(number, libc::F_GETFD) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EBADF) {
                continue;
            }
            return Err(query_error("fcntl(F_GETFD)", number, error));
        }
        if matches(number, expected)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn socket_name_error(number: RawFd, error: std::io::Error) -> std::io::Error {
    let failure = query_error("getsockname", number, error);
    let mut status = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: fstat only queries the number and writes to initialized, aligned storage.
    let file_type = if unsafe { libc::fstat(number, status.as_mut_ptr()) } == -1 {
        format!("errno={}", std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        // SAFETY: the successful query initialized stat; only its mode is read.
        format!("{:#o}", unsafe { status.assume_init() }.st_mode & libc::S_IFMT)
    };
    // SAFETY: F_GETFD reads flags without changing or taking ownership of the descriptor.
    let flags = unsafe { libc::fcntl(number, libc::F_GETFD) };
    let close_on_exec = if flags == -1 {
        format!("errno={}", std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        (flags & libc::FD_CLOEXEC != 0).to_string()
    };
    let socket_type =
        integer_option(number, libc::SOL_SOCKET, libc::SO_TYPE, "getsockopt(SO_TYPE)")
            .map(|value| value.to_string())
            .unwrap_or_else(|error| format!("error({error})"));
    let mut peer = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut length = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: getpeername only queries the raw number and writes within the supplied buffer.
    let peer_errno =
        if unsafe { libc::getpeername(number, peer.as_mut_ptr().cast(), &raw mut length) } == -1 {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        } else {
            0
        };
    std::io::Error::other(format!(
        "{failure} fstat_type={file_type} cloexec={close_on_exec} so_type={socket_type} getpeername_errno={peer_errno}"
    ))
}

fn query_error(query: &str, number: RawFd, error: std::io::Error) -> std::io::Error {
    std::io::Error::other(format!(
        "{query} fd={number} errno={}",
        error.raw_os_error().unwrap_or(0)
    ))
}

fn integer_option(
    number: RawFd,
    level: libc::c_int,
    option: libc::c_int,
    query: &str,
) -> std::io::Result<i32> {
    let mut value = 0_i32;
    let mut length = std::mem::size_of_val(&value) as libc::socklen_t;
    // SAFETY: the integer is initialized and writable for the supplied size;
    // a query error is propagated, never interpreted as a listening socket.
    if unsafe { libc::getsockopt(number, level, option, (&raw mut value).cast(), &raw mut length) }
        == -1
    {
        return Err(query_error(query, number, std::io::Error::last_os_error()));
    }
    if length as usize != std::mem::size_of_val(&value) {
        return Err(query_error(
            query,
            number,
            std::io::Error::other("invalid socket option size"),
        ));
    }
    Ok(value)
}

#[cfg(not(target_vendor = "apple"))]
fn listening(number: RawFd) -> std::io::Result<bool> {
    Ok(integer_option(number, libc::SOL_SOCKET, libc::SO_ACCEPTCONN, "getsockopt(SO_ACCEPTCONN)")?
        == 1)
}

#[cfg(target_vendor = "apple")]
fn listening(number: RawFd) -> std::io::Result<bool> {
    let mut info = std::mem::MaybeUninit::<libc::tcp_connection_info>::zeroed();
    let size = std::mem::size_of::<libc::tcp_connection_info>();
    let mut length = size as libc::socklen_t;
    // SAFETY: the initialized, aligned buffer is writable for size bytes; the
    // TCP-level query reports state without changing or owning the descriptor.
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
        return Err(query_error(
            "getsockopt(TCP_CONNECTION_INFO)",
            number,
            std::io::Error::last_os_error(),
        ));
    }
    if !(1..=size).contains(&(length as usize)) {
        return Err(query_error(
            "getsockopt(TCP_CONNECTION_INFO)",
            number,
            std::io::Error::other("invalid TCP connection-info size"),
        ));
    }
    // SAFETY: the buffer was initialized and the returned prefix contains state.
    Ok(unsafe { info.assume_init() }.tcpi_state == 1)
}
