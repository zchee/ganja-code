use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::fd::{AsFd as _, AsRawFd as _, IntoRawFd as _};
use std::time::Duration;

use super::*;

fn expected() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 4096))
}

#[test]
fn standard_descriptors_are_refused_without_being_closed() {
    for fd in -1..3 {
        // SAFETY: F_GETFD only queries the number and fails for closed inputs.
        let before = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(
            take(fd, expected())
                .expect_err("standard descriptor")
                .to_string()
                .contains("at least 3")
        );
        // SAFETY: no ownership has been transferred to the helper on refusal.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, before);
    }
}

#[test]
fn a_closed_descriptor_is_an_ordinary_error() {
    let error = take(i32::MAX, expected()).expect_err("closed number");
    assert!(error.to_string().contains("not open"), "{error:#}");
}

#[test]
fn a_regular_file_is_refused_without_being_closed() {
    let file = tempfile::tempfile().expect("regular file");
    let error = take(file.as_raw_fd(), expected()).expect_err("not a socket");
    assert!(error.to_string().contains("socket"), "{error:#}");
    file.metadata().expect("the rejected file is still owned by its caller");
}

#[test]
fn a_connected_stream_is_refused_without_being_closed() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
    let address = listener.local_addr().expect("address");
    let stream = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3))
        .expect("connected client");
    let error = take(stream.as_raw_fd(), stream.local_addr().expect("client address"))
        .expect_err("connected streams are not listening sockets");
    assert!(error.to_string().contains("not listening"), "{error:#}");
    stream.peer_addr().expect("the caller's connected stream stays open");
}

#[tokio::test]
async fn a_bound_but_non_listening_stream_is_refused_without_being_closed() {
    let socket = tokio::net::TcpSocket::new_v4().expect("unconnected stream socket");
    socket.bind((Ipv4Addr::LOCALHOST, 0).into()).expect("bind without listen");
    let address = socket.local_addr().expect("bound address");
    let error = take(socket.as_raw_fd(), address).expect_err("bound does not mean listening");
    assert!(error.to_string().contains("not listening"), "{error:#}");
    assert_eq!(socket.local_addr().expect("still owned by caller"), address);
}

#[test]
fn a_datagram_socket_is_refused_without_being_closed() {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("datagram socket");
    let address = socket.local_addr().expect("address");
    let error = take(socket.as_raw_fd(), address).expect_err("not a stream socket");
    assert!(error.to_string().contains("not a stream socket"), "{error:#}");
    assert_eq!(socket.local_addr().expect("still owned by caller"), address);
}

#[test]
fn a_listening_unix_stream_is_refused_without_being_closed() {
    let home = tempfile::tempdir().expect("Unix listener directory");
    let socket = std::os::unix::net::UnixListener::bind(home.path().join("listener.sock"))
        .expect("listening Unix stream");
    let error = take(socket.as_raw_fd(), expected()).expect_err("not an Internet stream");
    assert!(error.to_string().contains("not an IPv4 or IPv6 socket"), "{error:#}");
    socket.local_addr().expect("the caller still owns its Unix listener");
}

#[test]
fn an_unsupported_socket_query_is_an_ordinary_error() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
    let error = socket_option(listener.as_raw_fd(), libc::c_int::MAX)
        .expect_err("unsupported socket options cannot become a listening answer");
    assert!(error.to_string().contains("usable socket"), "{error:#}");
    listener.local_addr().expect("query error did not close the listener");
}

#[test]
fn an_address_mismatch_is_refused_without_closing_the_listener() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
    let address = listener.local_addr().expect("address");
    let other = SocketAddr::from((Ipv4Addr::UNSPECIFIED, address.port()));
    let error = take(listener.as_raw_fd(), other).expect_err("wrong host");
    assert!(error.to_string().contains("does not match"), "{error:#}");
    assert_eq!(listener.local_addr().expect("still open"), address);
}

#[test]
fn a_port_mismatch_is_refused_without_closing_the_listener() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
    let address = listener.local_addr().expect("address");
    let other = SocketAddr::new(address.ip(), address.port().wrapping_add(1));
    let error = take(listener.as_raw_fd(), other).expect_err("wrong port");
    assert!(error.to_string().contains("does not match"), "{error:#}");
    assert_eq!(listener.local_addr().expect("still open"), address);
}

#[tokio::test]
async fn an_adopted_listener_is_nonblocking_and_close_on_exec() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
    let address = listener.local_addr().expect("address");
    let flags = rustix::io::fcntl_getfd(listener.as_fd()).expect("descriptor flags");
    rustix::io::fcntl_setfd(listener.as_fd(), flags - rustix::io::FdFlags::CLOEXEC)
        .expect("simulate exec inheritance");
    let listener =
        take(listener.into_raw_fd(), address).expect("adopt the unowned inherited socket");
    assert_eq!(listener.local_addr().expect("address"), address);
    assert!(
        rustix::io::fcntl_getfd(listener.as_fd())
            .expect("resealed flags")
            .contains(rustix::io::FdFlags::CLOEXEC)
    );
    // SAFETY: querying status flags does not change ownership of this live handle.
    assert_ne!(unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK, 0);
}

#[tokio::test]
async fn a_reused_descriptor_is_refused_without_closing_the_new_resource() {
    const CHILD: &str = "GANJA_TEST_REUSED_LISTENER_FD";
    if std::env::var_os(CHILD).is_some() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("old listener");
        let number = listener.as_raw_fd();
        drop(listener);
        let file = std::fs::File::open("/dev/null").expect("startup reused descriptor");
        assert_eq!(file.as_raw_fd(), number, "the rejected number really was reused");
        let error = take(number, expected()).expect_err("not a listening socket any more");
        file.metadata().expect("refusal did not close the new resource");
        drop(file);
        eprintln!("{error:#}");
        std::process::exit(1);
    }
    let stderr = tempfile::NamedTempFile::new().expect("child diagnostics");
    let mut child = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "inherited_listener::tests::a_reused_descriptor_is_refused_without_closing_the_new_resource", "--nocapture"])
        .env(CHILD, "1")
        .stderr(stderr.reopen().expect("diagnostics handle"))
        .kill_on_drop(true)
        .spawn().expect("real process");
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    if status.is_err() {
        child.start_kill().expect("kill timed out child");
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("bounded cleanup")
            .expect("reap child");
    }
    assert_eq!(
        status.expect("ordinary refusal finishes").expect("child exit").code(),
        Some(1),
        "the process returns an error, never an I/O-safety abort"
    );
    let message = std::fs::read_to_string(stderr.path()).expect("diagnostics");
    assert!(message.contains("socket"), "{message}");
}
