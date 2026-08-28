use std::sync::Arc;
use std::sync::atomic::Ordering;

use ganja_core::Engine;
use ganja_core::provider::FakeProvider;
use ganja_protocol::Command;

use super::fake::{Recording, served};
use super::{SessionSocket, Synced};

fn engine() -> Arc<Engine> {
    Arc::new(Engine::new(
        Arc::new(FakeProvider::default()),
        "fake",
        Arc::new(ganja_tool::Registry::new(Vec::new())),
        ganja_permission::Permissions::default(),
    ))
}

#[tokio::test]
async fn the_socket_follows_the_session_slot_and_is_bound_once_per_id() {
    let engine = engine();
    let recording = Arc::new(Recording::default());
    let mut socket = SessionSocket::new(Box::new(Arc::clone(&recording)), served());

    let first = engine.session_id();
    assert_eq!(
        socket.sync(&engine).await,
        Synced::Bound(Recording::path_for(&first)),
        "the first pass binds under the engine's id"
    );
    assert_eq!(socket.sync(&engine).await, Synced::Unchanged);
    assert_eq!(socket.sync(&engine).await, Synced::Unchanged);
    assert_eq!(
        recording.binds.load(Ordering::SeqCst),
        1,
        "a pass over an unmoved slot binds nothing"
    );

    engine.send(Command::NewSession).await.expect("a fresh session");
    let second = engine.session_id();
    assert_ne!(first, second, "NewSession re-mints the id");
    assert_eq!(
        socket.sync(&engine).await,
        Synced::Bound(Recording::path_for(&second)),
        "the slot moved, so the socket moved"
    );
    assert_eq!(
        recording.closed.lock().expect("not poisoned").as_slice(),
        &[Recording::path_for(&first)],
        "the old socket was shut down before the new one was bound"
    );
    assert_eq!(
        recording.bound.lock().expect("not poisoned").as_slice(),
        &[first.clone(), second.clone()]
    );

    socket.shutdown().await;
    assert_eq!(socket.path(), None);
    assert_eq!(
        recording.closed.lock().expect("not poisoned").len(),
        2,
        "the exit path shuts the bound socket down"
    );
    socket.shutdown().await;
    assert_eq!(
        recording.closed.lock().expect("not poisoned").len(),
        2,
        "a second shutdown has nothing to shut down"
    );
}

#[tokio::test]
async fn a_refused_bind_is_a_sentence_not_retried_until_the_slot_moves() {
    let engine = engine();
    let recording = Arc::new(Recording::default());
    recording.refuse.store(true, Ordering::SeqCst);
    let mut socket = SessionSocket::new(Box::new(Arc::clone(&recording)), served());

    assert_eq!(
        socket.sync(&engine).await,
        Synced::Refused("no session socket: the directory is not ours".to_owned())
    );
    assert_eq!(socket.sync(&engine).await, Synced::Unchanged);
    assert_eq!(recording.binds.load(Ordering::SeqCst), 1, "the same id is not asked for again");
    assert_eq!(socket.path(), None, "nothing is bound");

    recording.refuse.store(false, Ordering::SeqCst);
    assert_eq!(
        socket.sync(&engine).await,
        Synced::Unchanged,
        "and still not, while the slot stands"
    );
    engine.send(Command::NewSession).await.expect("a fresh session");
    assert!(
        matches!(socket.sync(&engine).await, Synced::Bound(_)),
        "a moved slot is a new question"
    );
    socket.shutdown().await;
}
