//! The catalog with fetching switched off and nothing cached.
//!
//! The compiled-in snapshot is unconditional, which is the whole reason a
//! session can be started on a train: sizing and pricing must answer with no
//! network, no cache and nothing to fall back on but the binary itself.
//!
//! One test, in its own binary, because it turns fetching off for the whole
//! process. The socket it points the catalog at is real and is expected to
//! stay untouched — "no fetch happened" is asserted against a listener that
//! would have counted one, not against the absence of a log line.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ganja_core::catalog;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn the_static_table_answers_when_the_network_never_does() {
    let cache_home = tempfile::tempdir().expect("a temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
    let url = format!("http://{}", listener.local_addr().expect("a bound socket has an address"));

    // Anything that reaches this socket is a fetch that should not have
    // happened; the count is what the assertions below read.
    let reached = Arc::new(AtomicUsize::new(0));
    let _server = tokio::spawn({
        let reached = Arc::clone(&reached);
        async move {
            while listener.accept().await.is_ok() {
                reached.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", cache_home.path());
        std::env::set_var(catalog::MODELS_URL_ENV, &url);
        std::env::set_var(catalog::DISABLE_FETCH_ENV, "1");
        std::env::remove_var(catalog::MODELS_PATH_ENV);
    }

    // Neither the forced refresh nor the startup loop has anything to do:
    // fetching is off, and there is no cache to adopt.
    assert!(
        !catalog::refresh(true).await.expect("nothing failed"),
        "a forced refresh with fetching off is a no-op, not an error"
    );
    assert!(!catalog::load_cached(), "there is no cache to adopt");

    let cancel = CancellationToken::new();
    catalog::spawn_refresh_loop(cancel.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;
    cancel.cancel();

    assert_eq!(
        reached.load(Ordering::SeqCst),
        0,
        "nothing may reach the network when fetching is disabled"
    );

    // And every question a session asks the catalog still has an answer.
    let sonnet = catalog::model("claude-sonnet-5").expect("the snapshot carries sonnet");
    assert_eq!(sonnet.provider_id, "anthropic");
    assert_eq!(sonnet.context_window, 1_000_000);
    assert_eq!(sonnet.max_output, 128_000);
    assert!((sonnet.pricing.input - 2.0).abs() < f64::EPSILON);
    assert_eq!(sonnet.pricing.cache_write, Some(2.5));

    assert_eq!(
        catalog::default_model("anthropic"),
        Some("claude-opus-4-8"),
        "the provider a session selects still resolves to a model"
    );
    assert!(
        catalog::models().count() >= 10,
        "the whole snapshot is in the table, not just the row asked for"
    );

    // The compaction trigger and the reply cap are what the window and the
    // output limit feed; both being real numbers is what makes the offline
    // session behave like any other.
    assert!(catalog::models().all(|model| model.context_window > 0 && model.max_output > 0));
}
