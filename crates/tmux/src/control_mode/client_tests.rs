use futures::StreamExt;

use super::*;
use crate::control_mode::commandline::{Arg, Command};

type PeerRead = tokio::io::ReadHalf<tokio::io::DuplexStream>;
type PeerWrite = tokio::io::WriteHalf<tokio::io::DuplexStream>;

/// One end of a scripted [`Client`] plus the split peer halves a test
/// drives directly — kept as two independent owned values (rather than
/// two borrows of one `DuplexStream`) so a test can hold `client` and
/// `peer_write`/`peer_read` in concurrent futures without the borrow
/// checker treating them as aliasing one struct.
struct Scripted {
    client: Client,
    peer_read: PeerRead,
    peer_write: PeerWrite,
}

fn scripted_client(options: Options) -> Scripted {
    let (client_end, peer) = tokio::io::duplex(8192);
    let client = Client::from_duplex(&options, client_end, None);
    let (peer_read, peer_write) = tokio::io::split(peer);
    Scripted { client, peer_read, peer_write }
}

/// Writes `line + "\n"` on the peer end, as if tmux had sent it.
async fn peer_send(write: &mut PeerWrite, line: &str) {
    use tokio::io::AsyncWriteExt;
    write.write_all(line.as_bytes()).await.unwrap();
    write.write_all(b"\n").await.unwrap();
    write.flush().await.unwrap();
}

/// Reads one newline-framed line the client wrote, as tmux would.
async fn peer_recv_written(read: &mut PeerRead) -> String {
    let mut reader = tokio::io::BufReader::new(read);
    let mut buf = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut buf).await.unwrap();
    trim_line_ending(&buf).to_string()
}

fn default_options() -> Options {
    Options::new().with_session_name("test").with_shutdown_timeout(Duration::from_millis(200))
}

#[tokio::test]
async fn exec_serializes_and_routes_responses_by_writing_then_answering() {
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client(default_options());
    let exec = async {
        client
            .exec(Command::from_static("display-message"), [Arg::raw("-p"), Arg::string("hello")])
            .await
    };
    let answer = async {
        let written = peer_recv_written(&mut peer_read).await;
        assert_eq!(written, "display-message -p hello");
        peer_send(&mut peer_write, "%begin 1 2 1").await;
        peer_send(&mut peer_write, "hello").await;
        peer_send(&mut peer_write, "%end 1 2 1").await;
    };
    let (result, ()) = tokio::join!(exec, answer);
    let response = result.unwrap();
    assert_eq!(response.lines, vec!["hello".to_string()]);
}

#[tokio::test]
async fn a_percent_error_response_becomes_a_command_error() {
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client(default_options());
    let exec = async { client.exec_raw("bad-command").await };
    let answer = async {
        let written = peer_recv_written(&mut peer_read).await;
        assert_eq!(written, "bad-command");
        peer_send(&mut peer_write, "%begin 1 3 1").await;
        peer_send(&mut peer_write, "parse error").await;
        peer_send(&mut peer_write, "%error 1 3 1").await;
    };
    let (result, ()) = tokio::join!(exec, answer);
    let err = result.unwrap_err();
    let Error::Command(command_err) = err else {
        panic!("expected Error::Command, got {err:?}");
    };
    assert_eq!(command_err.line, "bad-command");
    assert_eq!(command_err.response.lines, vec!["parse error".to_string()]);
}

#[tokio::test]
async fn concurrent_execs_are_serialized_onto_one_pending_slot() {
    let Scripted { client, peer_read, peer_write } = scripted_client(default_options());
    let client = std::sync::Arc::new(client);

    let responder = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(peer_read);
        let mut writer = peer_write;
        for id in 1..=8i64 {
            let mut buf = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut buf).await.unwrap();
            let reply = format!("%begin 1 {id} 1\n{}\n%end 1 {id} 1\n", trim_line_ending(&buf));
            tokio::io::AsyncWriteExt::write_all(&mut writer, reply.as_bytes()).await.unwrap();
            tokio::io::AsyncWriteExt::flush(&mut writer).await.unwrap();
        }
    });

    let mut handles = Vec::new();
    for i in 0..8 {
        let client = std::sync::Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            client.exec_raw(&format!("display-message -p {i}")).await
        }));
    }
    // The responder echoes each request line back as the reply body, so
    // every exec must read back its *own* command whatever order the
    // eight reached the wire in — a client that answered all eight from
    // a misrouted slot would still return Ok, which is why Ok alone was
    // not the claim.
    for (i, handle) in handles.into_iter().enumerate() {
        let response = handle.await.unwrap().unwrap();
        assert_eq!(
            response.lines,
            vec![format!("display-message -p {i}")],
            "exec {i} must be answered by the reply to its own command"
        );
    }
    responder.await.unwrap();
}

#[tokio::test]
async fn dropping_the_exec_future_after_a_successful_write_poisons_the_client() {
    let Scripted { client, mut peer_read, peer_write: _peer_write } =
        scripted_client(default_options());
    let client = std::sync::Arc::new(client);

    let exec_client = std::sync::Arc::clone(&client);
    let handle = tokio::spawn(async move {
        let _ = exec_client.exec_raw("display-message -p wait").await;
    });

    // Prove the write actually reached the peer before we cut the
    // future off, so this exercises the "dropped while waiting for the
    // response" edge rather than the "dropped before the write" one.
    let written = peer_recv_written(&mut peer_read).await;
    assert_eq!(written, "display-message -p wait");

    // Abort the task without ever answering it — the exec future is
    // dropped mid-flight, which is this port's cancellation signal.
    handle.abort();
    let _ = handle.await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let err = client.exec_raw("display-message -p after").await.unwrap_err();
    assert!(matches!(err, Error::Closed));
}

#[tokio::test]
async fn a_drop_while_blocked_mid_write_poisons_the_client() {
    // A 4-byte duplex with nobody draining it: `write_all` cannot
    // finish a payload larger than that without genuinely suspending
    // inside its own `.await`, which is exactly the window Finding 2
    // covers — a drop landing there, before the write has visibly
    // completed.
    let (client_end, mut peer) = tokio::io::duplex(4);
    let client = Client::from_duplex(&default_options(), client_end, None);
    let client = std::sync::Arc::new(client);

    let line = format!("display-message -p {}", "x".repeat(64));
    let exec_client = std::sync::Arc::clone(&client);
    let handle = tokio::spawn(async move { exec_client.exec_raw(&line).await });

    // Deterministically establish the write is genuinely mid-flight:
    // drain exactly the duplex's own capacity, then stop — the writer
    // now holds undelivered bytes and is parked inside `write_all`'s
    // own `.await`, not merely scheduled to run it.
    let mut sink = [0u8; 4];
    tokio::io::AsyncReadExt::read_exact(&mut peer, &mut sink).await.unwrap();

    // Abort the task without ever draining further — the exec future
    // is dropped while genuinely suspended inside the write itself.
    handle.abort();
    let _ = handle.await;

    let err = client.exec_raw("display-message -p after").await.unwrap_err();
    assert!(matches!(err, Error::Closed));
}

#[tokio::test]
async fn a_second_exec_after_a_completed_one_still_works() {
    let Scripted { client, mut peer_read, mut peer_write } = scripted_client(default_options());
    {
        let exec = async { client.exec_raw("display-message -p one").await };
        let answer = async {
            let _ = peer_recv_written(&mut peer_read).await;
            peer_send(&mut peer_write, "%begin 1 1 1").await;
            peer_send(&mut peer_write, "%end 1 1 1").await;
        };
        let (result, ()) = tokio::join!(exec, answer);
        result.unwrap();
    }
    let exec = async { client.exec_raw("display-message -p two").await };
    let answer = async {
        let written = peer_recv_written(&mut peer_read).await;
        assert_eq!(written, "display-message -p two");
        peer_send(&mut peer_write, "%begin 1 2 1").await;
        peer_send(&mut peer_write, "%end 1 2 1").await;
    };
    let (result, ()) = tokio::join!(exec, answer);
    result.unwrap();
}

#[tokio::test]
async fn notifications_beyond_the_buffer_drop_the_oldest_and_count() {
    let Scripted { client, peer_read: _peer_read, mut peer_write } =
        scripted_client(default_options().with_event_buffer(1));
    peer_send(&mut peer_write, "%message first").await;
    peer_send(&mut peer_write, "%message second").await;
    peer_send(&mut peer_write, "%message third").await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while client.dropped_notifications() != 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(client.dropped_notifications(), 2);

    let got = tokio::time::timeout(Duration::from_secs(1), client.recv()).await.unwrap().unwrap();
    assert_eq!(got.raw, "%message third");
}

#[tokio::test]
async fn an_exit_notification_ends_the_read_loop_and_the_event_stream() {
    let Scripted { client, peer_read: _peer_read, mut peer_write } =
        scripted_client(default_options());
    peer_send(&mut peer_write, "%exit detached").await;
    peer_send(&mut peer_write, "%message stray after exit").await;

    let mut events = Vec::new();
    let stream = client.events();
    tokio::pin!(stream);
    while let Ok(Some(notification)) =
        tokio::time::timeout(Duration::from_millis(200), stream.next()).await
    {
        events.push(notification);
    }
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, NotificationKind::Exit);
}

#[tokio::test]
async fn a_parser_error_synthesizes_a_protocol_error_notification_before_aborting() {
    let Scripted { client, peer_read: _peer_read, mut peer_write } =
        scripted_client(default_options());
    peer_send(&mut peer_write, "stray line outside any block").await;

    let notification =
        tokio::time::timeout(Duration::from_secs(1), client.recv()).await.unwrap().unwrap();
    assert_eq!(notification.kind, NotificationKind::ProtocolError);
    assert_eq!(notification.raw, "stray line outside any block");

    // The client is closed, but with the *specific* abort cause stored
    // (Go's closedError returns the stored cause when one is set,
    // defaulting to a bare "closed" only when none was) — the protocol
    // error itself, not a generic Closed.
    let err = client.exec_raw("display-message -p x").await.unwrap_err();
    assert!(matches!(err, Error::Protocol(_)));
}

#[tokio::test]
async fn close_is_idempotent() {
    let scripted = scripted_client(default_options());
    let first = scripted.client.close().await;
    let second = scripted.client.close().await;
    assert_eq!(first.is_ok(), second.is_ok());
    if let (Err(a), Err(b)) = (&first, &second) {
        assert_eq!(a.to_string(), b.to_string());
    }
}

#[tokio::test]
async fn detach_is_skipped_and_reported_when_the_write_lock_is_held() {
    let scripted = scripted_client(default_options());
    // Hold the write lock across the whole close by starting (and
    // never answering) an exec, matching the write-lock-held scenario
    // the DetachSkippedWriteLocked variant exists for.
    let write_guard = scripted.client.write.lock().await;
    let close_result = scripted.client.close().await;
    drop(write_guard);

    let Err(Error::Close { errors }) = close_result else {
        panic!("expected Error::Close, got {close_result:?}");
    };
    assert!(errors.iter().any(|e| matches!(e, Error::DetachSkippedWriteLocked)));
}

#[tokio::test]
async fn close_unblocks_a_pending_exec_holding_the_write_lock() {
    let Scripted { client, mut peer_read, peer_write: _peer_write } =
        scripted_client(default_options());
    let client = std::sync::Arc::new(client);

    let exec_client = std::sync::Arc::clone(&client);
    let exec = tokio::spawn(async move { exec_client.exec_raw("display-message -p wait").await });

    // Wait for the write to land before closing, so the write lock is
    // genuinely held by the in-flight exec rather than merely
    // registered.
    let written = peer_recv_written(&mut peer_read).await;
    assert_eq!(written, "display-message -p wait");

    let close_result = client.close().await;
    let Err(Error::Close { errors }) = close_result else {
        panic!("expected Error::Close, got {close_result:?}");
    };
    assert!(errors.iter().any(|e| matches!(e, Error::DetachSkippedWriteLocked)));

    // `close`'s `abort` fails the pending registration before it ever
    // attempts the write lock, so the blocked exec unblocks with
    // `Closed` even though `close` itself could not reach the writer.
    let exec_result = tokio::time::timeout(Duration::from_secs(1), exec)
        .await
        .expect("exec_raw did not unblock after close")
        .unwrap();
    assert!(matches!(exec_result, Err(Error::Closed)));
}

/// Ports Go's `TestClientCloseUnblocksExecRawStuckInTransportWrite`.
/// Finding 1 (W2 review): a write genuinely blocked inside the
/// transport — not merely awaiting a response with the write already
/// on the wire, as the sibling test above covers — used to hang
/// forever behind a concurrent `close()`, since `close_inner` skips
/// the writer on a failed `try_lock` and, in `from_duplex` mode, has
/// no child process to kill to force it. See `Client::close`'s doc.
#[tokio::test]
async fn close_unblocks_an_exec_blocked_inside_write_all() {
    let (client_end, mut peer) = tokio::io::duplex(4);
    let client = Client::from_duplex(&default_options(), client_end, None);
    let client = std::sync::Arc::new(client);

    let line = format!("display-message -p {}", "x".repeat(64));
    let exec_client = std::sync::Arc::clone(&client);
    let exec = tokio::spawn(async move { exec_client.exec_raw(&line).await });

    // Same deterministic mid-write technique as
    // `a_drop_while_blocked_mid_write_poisons_the_client`: drain
    // exactly the duplex's own capacity, then stop — the exec call is
    // now genuinely blocked inside `write_all`, holding the write
    // lock, rather than merely registered as pending.
    let mut sink = [0u8; 4];
    tokio::io::AsyncReadExt::read_exact(&mut peer, &mut sink).await.unwrap();

    let close_result = client.close().await;
    let Err(Error::Close { errors }) = close_result else {
        panic!("expected Error::Close, got {close_result:?}");
    };
    assert!(errors.iter().any(|e| matches!(e, Error::DetachSkippedWriteLocked)));

    let exec_result = tokio::time::timeout(Duration::from_secs(1), exec)
        .await
        .expect("exec_raw did not unblock after close")
        .unwrap();
    assert!(matches!(exec_result, Err(Error::Closed)));
}

#[tokio::test]
async fn close_after_read_eof_still_succeeds() {
    // Deliberately not `scripted_client`: that helper `tokio::io::split`s
    // the peer end, and a split half only shares — never solely owns —
    // the underlying stream, so dropping just one half never closes it.
    // An unsplit `peer` does: dropping it here drops the *only* handle
    // to that end, which the client observes as EOF on its own read half.
    let (client_end, peer) = tokio::io::duplex(8192);
    let client = Client::from_duplex(&default_options(), client_end, None);
    drop(peer);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while client.shared.closed_error().is_none() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(client.shared.closed_error().is_some());

    client.close().await.unwrap();
}

#[tokio::test]
async fn a_second_close_call_observes_the_first_calls_finished_result_rather_than_racing_it() {
    let scripted = scripted_client(default_options());
    let client = std::sync::Arc::new(scripted.client);

    let first_client = std::sync::Arc::clone(&client);
    let first = tokio::spawn(async move { first_client.close().await });
    let second_client = std::sync::Arc::clone(&client);
    let second = tokio::spawn(async move { second_client.close().await });

    let (first_result, second_result) = tokio::join!(first, second);
    let first_result = first_result.unwrap();
    let second_result = second_result.unwrap();
    assert_eq!(first_result.is_ok(), second_result.is_ok());
    if let (Err(a), Err(b)) = (&first_result, &second_result) {
        assert_eq!(a.to_string(), b.to_string());
    }
}

#[tokio::test]
async fn stderr_tail_is_bounded_and_populated() {
    let options = default_options().with_stderr_line_limit(2);
    let (client_end, _peer) = tokio::io::duplex(8192);
    let (stderr_client_end, mut stderr_peer) = tokio::io::duplex(8192);
    let client = Client::from_duplex(&options, client_end, Some(stderr_client_end));

    use tokio::io::AsyncWriteExt;
    stderr_peer.write_all(b"one\ntwo\nthree\n").await.unwrap();
    stderr_peer.flush().await.unwrap();
    drop(stderr_peer);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while client.stderr_tail().len() < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(client.stderr_tail(), vec!["two".to_string(), "three".to_string()]);
}

#[tokio::test]
async fn trim_line_ending_strips_lf_then_optional_cr() {
    assert_eq!(trim_line_ending("abc\r\n"), "abc");
    assert_eq!(trim_line_ending("abc\n"), "abc");
    assert_eq!(trim_line_ending("abc"), "abc");
}
