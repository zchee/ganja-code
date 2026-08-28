//! An expiring token is renewed once, however many callers notice at once.
//!
//! A turn can have several requests in the air, and they all read the same
//! credential. If each of them renewed it, the second exchange would present a
//! refresh token the first has already spent — xAI's rotate
//! (`plugin/xai.ts:500`, which stores the rotated pair) — and the provider
//! would be right to refuse it, logging the person out mid-turn. Upstream
//! guards this with a module-scoped promise per plugin (`xai.ts:494-521`,
//! `openai/codex.ts:362-386`); this is that guarantee, keyed by provider.
//!
//! Time is paused, which is what makes the proof a proof rather than a race:
//! the fake renewal parks on a timer, so every caller is inside the window when
//! the assertion is taken. With the coalescing removed this counts one call per
//! caller, deterministically.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and `cargo test` runs the tests inside a binary on parallel
//! threads.

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ganja_core::auth::{self, AuthError, AuthErrorKind, OauthCredential, RefreshOauth, Refresher};
use secrecy::{ExposeSecret as _, SecretString};

/// Stored under upstream's key for xAI even though ganja calls the provider
/// `grok`, which is the point of [`auth::storage_key`].
const PROVIDER: &str = "grok";

/// How long the fake renewal takes. Any non-zero duration holds the window
/// open under paused time; five seconds is just legible in a log.
const RENEWAL: Duration = Duration::from_secs(5);

/// Callers that discover the expiry at the same moment.
const CALLERS: usize = 8;

/// Renews a credential, and counts how many times it was asked to.
#[derive(Default)]
struct CountingRefresher {
    calls: AtomicUsize,
}

impl CountingRefresher {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl RefreshOauth for CountingRefresher {
    async fn refresh(
        &self,
        _provider_id: &str,
        _credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // What a token endpoint costs. Under paused time this is what keeps
        // every caller inside the window rather than finishing before the next
        // one starts.
        tokio::time::sleep(RENEWAL).await;

        Ok(OauthCredential::new(
            SecretString::from("rt-renewed-0003"),
            SecretString::from("at-renewed-0004"),
            auth::now_ms() + 3_600_000,
        ))
    }
}

/// A credential due for renewal, carrying an account id and a field this build
/// does not model — both of which a renewal has to carry forward, because a
/// token endpoint that does not echo them back has not revoked them.
fn expiring() -> OauthCredential {
    let mut credential = OauthCredential::new(
        SecretString::from("rt-original-0001"),
        SecretString::from("at-original-0002"),
        1,
    );
    credential.account_id = Some("acct-42".to_owned());
    credential.extra.insert("someFuturePluginField".to_owned(), serde_json::json!(true));

    credential
}

#[tokio::test(start_paused = true)]
async fn a_renewal_happens_only_when_it_must_and_only_once() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
    }
    let refresher = Arc::new(CountingRefresher::default());

    // A credential that is still good is handed back without the renewal
    // endpoint being troubled at all.
    let mut live = expiring();
    live.expires = auth::now_ms() + 86_400_000;
    auth::set_oauth(PROVIDER, &live).expect("the credential stores");

    let held = Refresher::new()
        .usable(PROVIDER, refresher.clone())
        .await
        .expect("a live credential needs nothing");
    assert_eq!(held.access.expose_secret(), "at-original-0002");
    assert_eq!(refresher.calls(), 0, "a credential that is not due must not be renewed");

    // One that is due, discovered by everybody at once.
    auth::set_oauth(PROVIDER, &expiring()).expect("the expiring credential stores");
    let shared = Arc::new(Refresher::new());
    let callers: Vec<_> = (0..CALLERS)
        .map(|_| {
            let shared = shared.clone();
            let refresher = refresher.clone();
            tokio::spawn(async move { shared.usable(PROVIDER, refresher).await })
        })
        .collect();

    let renewed: Vec<OauthCredential> = futures::future::join_all(callers)
        .await
        .into_iter()
        .map(|joined| joined.expect("no caller panicked").expect("the renewal ran"))
        .collect();

    assert_eq!(
        refresher.calls(),
        1,
        "{CALLERS} callers coalesce onto one renewal; a race would spend the \
         refresh token {CALLERS} times and the provider would refuse all but \
         the first"
    );
    for credential in &renewed {
        assert_eq!(credential.access.expose_secret(), "at-renewed-0004");
        assert_eq!(credential.refresh.expose_secret(), "rt-renewed-0003");
        assert_eq!(
            credential.account_id.as_deref(),
            Some("acct-42"),
            "a renewal returns tokens, not an identity"
        );
        assert_eq!(
            credential.extra.get("someFuturePluginField"),
            Some(&serde_json::json!(true)),
            "nor does it revoke a field it has never heard of"
        );
    }

    // The renewed credential is on disk before any caller is given it, so the
    // next process does not start by renewing a token that has been replaced.
    let persisted =
        auth::oauth_for(PROVIDER).expect("the store reads").expect("the credential is still there");
    assert_eq!(persisted.access.expose_secret(), "at-renewed-0004");
    assert_eq!(persisted.account_id.as_deref(), Some("acct-42"));

    // And a later caller, arriving after the renewal has been retired, gets the
    // stored credential without a second renewal.
    let after =
        shared.usable(PROVIDER, refresher.clone()).await.expect("the renewed credential is good");
    assert_eq!(after.access.expose_secret(), "at-renewed-0004");
    assert_eq!(refresher.calls(), 1);

    // The process-wide one is what a login flow reaches for, and it reads the
    // same store: a caller that never saw the renewal still gets its result.
    let elsewhere = Refresher::shared()
        .usable(PROVIDER, refresher.clone())
        .await
        .expect("the stored credential is good");
    assert_eq!(elsewhere.access.expose_secret(), "at-renewed-0004");
    assert_eq!(refresher.calls(), 1);

    // A provider with no OAuth credential is told which situation it is in, and
    // the message says what fixes it rather than what is stored.
    auth::set_credential("anthropic", "sk-not-an-oauth-token").expect("a key stores");
    let refused = Refresher::shared()
        .usable("anthropic", refresher)
        .await
        .expect_err("an API key is not an OAuth credential");
    assert_eq!(refused.kind(), AuthErrorKind::NotOauth);
    assert!(
        refused.to_string().contains("ganja auth login anthropic")
            && !refused.to_string().contains("sk-not-an-oauth-token"),
        "the way out belongs in the message and the key never does: {refused}"
    );
}
