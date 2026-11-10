use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::FutureExt as _;

use super::*;

struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn an_interrupt_between_startup_stages_is_not_lost() {
    let mut interrupt: Option<Interrupt> = Some(async { Ok(()) }.boxed());
    let result = during_startup(&mut interrupt, async { 7 }).await;
    assert_eq!(result.unwrap_err().to_string(), "cloud connection interrupted");
}

#[tokio::test]
async fn the_same_interrupt_survives_a_completed_startup_stage() {
    let (send, receive) = tokio::sync::oneshot::channel();
    let mut interrupt: Option<Interrupt> = Some(
        async move {
            receive.await.expect("the test sends an interrupt");
            Ok(())
        }
        .boxed(),
    );
    assert_eq!(during_startup(&mut interrupt, async { 7 }).await.unwrap(), 7);
    send.send(()).unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(Arc::clone(&dropped));
    let result = during_startup(&mut interrupt, async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    })
    .await;
    assert_eq!(result.unwrap_err().to_string(), "cloud connection interrupted");
    assert!(dropped.load(Ordering::SeqCst), "interruption must drop the startup operation");
}

#[tokio::test]
async fn startup_without_an_interrupt_listener_preserves_its_result() {
    assert_eq!(during_startup(&mut None, async { 7 }).await.unwrap(), 7);
}
