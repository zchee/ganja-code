#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{
    env, fs,
    path::PathBuf,
    sync::{Mutex, MutexGuard, PoisonError},
};

use secrecy::{ExposeSecret as _, SecretString};
use tempfile::TempDir;

use super::{
    AuthError, AuthErrorKind, Credential, CredentialKind, Entry, KEY_VARS, OauthCredential,
    REFRESH_SKEW_MS, RedactedTail, Source, Store, ZeroExpiry, credential_for, key_var,
    list_providers, now_ms, provider_id_for_storage_key, set_credential, set_oauth, storage_key,
    store_path, zero_expiry,
};

/// A key that exists only to be hunted for in output. Nothing may print it
/// whole.
const CANARY: &str = "sk-canary-8842";

/// The key a lookup found, for a test that has to prove a whole key round
/// tripped rather than just its tail. [`Credential`] has no `PartialEq`, on
/// purpose, so this is how a test compares one.
fn key_of(credential: Option<Credential>) -> Option<String> {
    credential.map(|credential| credential.api_key.expose_secret().to_owned())
}

/// Serializes the tests that read or write process-wide environment
/// variables. `cargo test` runs a binary's tests on a thread pool, and
/// `set_var` is a process-wide mutation: without this, two tests setting
/// `ANTHROPIC_API_KEY` would see each other's values.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

fn environment() -> MutexGuard<'static, ()> {
    // A test that panicked while holding the lock has already failed; the
    // ones after it should still run against a known environment.
    ENVIRONMENT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sets or clears `name` for a test that holds [`environment`].
fn set_env(name: &str, value: Option<&str>) {
    // SAFETY: every caller holds the ENVIRONMENT lock, so no other test
    // thread is reading or writing the environment concurrently.
    unsafe {
        match value {
            Some(value) => env::set_var(name, value),
            None => env::remove_var(name),
        }
    }
}

/// Clears every provider's key variable, so a developer's own exported key
/// cannot make a test pass or fail.
fn clear_keys() {
    for (_, variable) in KEY_VARS {
        set_env(variable, None);
    }
}

fn store(directory: &TempDir) -> Store {
    Store::new(directory.path().join("auth.json"))
}

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// The instant the predicate tests read as "now", so that every deadline
/// below is a stated distance from one fixed point rather than from a
/// clock.
const NOW_MS: u64 = 1_785_000_000_000;

/// The same instant in whole seconds, which is the unit a JWT states its
/// claims in.
const NOW_S: u64 = NOW_MS / 1_000;

/// A JWS compact serialization issued at `issued_at` and expiring at
/// `expires_at`, both in seconds, signed by nobody.
///
/// The signature is a placeholder because it is never looked at — that is
/// the posture these tests exist to pin, not an omission.
///
/// `iat` and `nbf` are carried because they are the two other claims in a
/// real token that are *also* NumericDates, and because every caller below
/// gives them a value that makes reading one instead of `exp` a **wrong**
/// answer rather than no answer: a token still good for a day was issued
/// now, so a decode looking at `iat` calls it spent.
fn jwt(issued_at: u64, expires_at: u64) -> SecretString {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::json!({
            "iat": issued_at,
            "nbf": issued_at,
            "exp": expires_at,
        })
        .to_string(),
    );

    SecretString::from(format!("eyJhbGciOiJSUzI1NiJ9.{payload}.not-a-signature"))
}

/// A credential with `expires` stored and `access` as given.
fn credential(access: SecretString, expires: u64) -> OauthCredential {
    OauthCredential::new(SecretString::from("rt-anything"), access, expires)
}

/// Everything an error would print, the way `anyhow` renders one: the
/// message plus every cause it can be walked down to. A secret hiding in a
/// `#[source]` is as leaked as one in the message itself.
fn rendered(error: &AuthError) -> Vec<String> {
    let mut chain = vec![error.to_string()];
    let mut cause = std::error::Error::source(error);

    while let Some(next) = cause {
        chain.push(next.to_string());
        cause = next.source();
    }

    chain
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777
}

#[cfg(windows)]
mod windows_dacl {
    use std::{mem::size_of, ptr};

    use windows_sys::Win32::{
        Foundation::GENERIC_READ,
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAceEx, AddAccessDeniedAceEx,
            EqualSid, INHERITED_ACE, InitializeAcl, WinWorldSid,
        },
        Storage::FileSystem::{
            FILE_ALL_ACCESS, FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, FILE_READ_EA, READ_CONTROL,
            SYNCHRONIZE,
        },
    };

    use super::super::windows_acl::{OwnedSid, exposed_grantee_in_dacl, private_acl, process_user};

    #[derive(Clone, Copy)]
    enum Kind {
        Allow,
        Deny,
    }

    #[derive(Clone, Copy)]
    struct Entry<'a> {
        kind: Kind,
        mask: u32,
        flags: u32,
        sid: &'a OwnedSid,
    }

    struct TestAcl {
        words: Box<[usize]>,
    }

    impl TestAcl {
        fn as_ptr(&self) -> *const ACL {
            self.words.as_ptr().cast()
        }
    }

    fn acl(entries: &[Entry<'_>]) -> TestAcl {
        let bytes = entries.iter().fold(size_of::<ACL>(), |bytes, entry| {
            bytes + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + entry.sid.len() as usize
        });
        let words = bytes.div_ceil(size_of::<usize>());
        let mut acl = TestAcl {
            words: vec![0; words].into_boxed_slice(),
        };
        let pointer = acl.words.as_mut_ptr().cast();
        assert_ne!(
            // SAFETY: the aligned allocation is bytes bytes long.
            unsafe { InitializeAcl(pointer, bytes as u32, ACL_REVISION) },
            0,
            "the test ACL initializes: {}",
            std::io::Error::last_os_error()
        );

        for entry in entries {
            // SAFETY: the ACL was sized for every entry and each SID is
            // owned for the duration of the call.
            let added = unsafe {
                match entry.kind {
                    Kind::Allow => AddAccessAllowedAceEx(
                        pointer,
                        ACL_REVISION,
                        entry.flags,
                        entry.mask,
                        entry.sid.as_psid(),
                    ),
                    Kind::Deny => AddAccessDeniedAceEx(
                        pointer,
                        ACL_REVISION,
                        entry.flags,
                        entry.mask,
                        entry.sid.as_psid(),
                    ),
                }
            };
            assert_ne!(
                added,
                0,
                "the test ACE is added: {}",
                std::io::Error::last_os_error()
            );
        }
        acl
    }

    fn allow(sid: &OwnedSid, mask: u32) -> Entry<'_> {
        Entry {
            kind: Kind::Allow,
            mask,
            flags: 0,
            sid,
        }
    }

    #[test]
    fn the_written_dacl_is_one_full_control_grant_to_the_process_user() {
        let user = process_user().expect("the process has a user SID");
        let acl = private_acl(&user).expect("the private ACL builds");

        // SAFETY: private_acl owns an initialized ACL header.
        let header = unsafe { &*acl.as_ptr() };
        assert_eq!(header.AceCount, 1);

        let mut raw = ptr::null_mut();
        assert_ne!(
            // SAFETY: index zero exists by the assertion above.
            unsafe { windows_sys::Win32::Security::GetAce(acl.as_ptr(), 0, &mut raw) },
            0,
            "the owner ACE reads: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: AddAccessAllowedAceEx created an ACCESS_ALLOWED_ACE at
        // index zero, and its SID begins at SidStart.
        let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = ptr::addr_of!(ace.SidStart).cast_mut().cast();
        assert_eq!(ace.Mask, FILE_ALL_ACCESS);
        // SAFETY: both pointers identify valid SIDs kept alive here.
        assert_ne!(unsafe { EqualSid(sid, user.as_psid()) }, 0);
    }

    #[test]
    fn the_accepted_identity_set_and_ace_kinds_match_the_privacy_rule() {
        let user = process_user().expect("the process has a user SID");
        let system = OwnedSid::well_known(windows_sys::Win32::Security::WinLocalSystemSid)
            .expect("SYSTEM has a SID");
        let administrators =
            OwnedSid::well_known(windows_sys::Win32::Security::WinBuiltinAdministratorsSid)
                .expect("Administrators has a SID");
        let everyone = OwnedSid::well_known(WinWorldSid).expect("Everyone has a SID");

        let owner_only = acl(&[allow(&user, FILE_ALL_ACCESS)]);
        assert_eq!(
            exposed_grantee_in_dacl(owner_only.as_ptr()).expect("the ACL reads"),
            None
        );

        let inherited_system_set = acl(&[
            Entry {
                flags: INHERITED_ACE,
                ..allow(&user, FILE_ALL_ACCESS)
            },
            Entry {
                flags: INHERITED_ACE,
                ..allow(&system, FILE_GENERIC_READ)
            },
            Entry {
                flags: INHERITED_ACE,
                ..allow(&administrators, FILE_GENERIC_READ)
            },
        ]);
        assert_eq!(
            exposed_grantee_in_dacl(inherited_system_set.as_ptr())
                .expect("the inherited ACL reads"),
            None
        );

        let denied_everyone = acl(&[
            Entry {
                kind: Kind::Deny,
                mask: FILE_ALL_ACCESS,
                flags: 0,
                sid: &everyone,
            },
            allow(&user, FILE_ALL_ACCESS),
        ]);
        assert_eq!(
            exposed_grantee_in_dacl(denied_everyone.as_ptr()).expect("the deny ACL reads"),
            None,
            "a deny ACE cannot widen access"
        );

        let noise = acl(&[allow(
            &everyone,
            SYNCHRONIZE | READ_CONTROL | FILE_READ_ATTRIBUTES | FILE_READ_EA,
        )]);
        assert_eq!(
            exposed_grantee_in_dacl(noise.as_ptr()).expect("the metadata ACL reads"),
            None,
            "metadata-only access cannot read credential bytes"
        );

        let generic_read = acl(&[allow(&everyone, GENERIC_READ)]);
        let grantee = exposed_grantee_in_dacl(generic_read.as_ptr())
            .expect("the generic ACL reads")
            .expect("generic read is exposure");
        assert!(
            grantee.contains("Everyone") || grantee == "S-1-1-0",
            "the offending grantee should be named, got {grantee}"
        );

        assert_eq!(
            exposed_grantee_in_dacl(ptr::null())
                .expect("a NULL DACL is classified")
                .as_deref(),
            Some("Everyone (NULL DACL)")
        );
    }
}

#[test]
fn a_missing_store_reads_as_no_credentials_rather_than_an_error() {
    let directory = temporary();
    let store = store(&directory);

    assert_eq!(
        key_of(store.get("anthropic").expect("a missing file is fine")),
        None
    );
    assert!(store.stored().expect("a missing file is fine").is_empty());
    assert!(!store.remove("anthropic").expect("a missing file is fine"));
}

#[test]
fn a_stored_key_round_trips_and_can_be_forgotten() {
    let directory = temporary();
    let store = store(&directory);

    store.set("anthropic", CANARY).expect("the key stores");

    assert_eq!(
        key_of(store.get("anthropic").expect("the key reads back")),
        Some(CANARY.to_owned())
    );
    assert_eq!(
        store.stored().expect("the listing reads"),
        vec![(
            "anthropic".to_owned(),
            RedactedTail::of(CANARY),
            CredentialKind::ApiKey
        )]
    );

    assert!(store.remove("anthropic").expect("the key is removable"));
    assert_eq!(
        key_of(store.get("anthropic").expect("the file still reads")),
        None
    );
}

/// The file shape is upstream's, so that the two tools can eventually read
/// each other's storage.
#[test]
fn the_file_is_written_in_upstreams_shape() {
    let directory = temporary();
    let store = store(&directory);
    store.set("openai", CANARY).expect("the key stores");

    let written = fs::read_to_string(&store.path).expect("the file exists");
    let parsed: serde_json::Value = serde_json::from_str(&written).expect("the file is JSON");

    assert_eq!(parsed["openai"]["type"], "api");
    assert_eq!(parsed["openai"]["key"], CANARY);
}

/// Credentials this build cannot use — upstream's `wellknown` entries, a
/// credential type nobody has invented yet, providers it has never heard of
/// — survive a rewrite. Dropping them would silently log someone out of a
/// tool that is still using the same file.
///
/// The `oauth` entry that used to carry this assertion moved to
/// [`an_oauth_entry_round_trips_with_everything_it_arrived_with`]: it is a
/// credential this build understands now, so it can no longer stand for one
/// it does not.
#[test]
fn foreign_entries_survive_a_rewrite() {
    let directory = temporary();
    let store = store(&directory);
    let original = serde_json::json!({
        "anthropic": { "type": "wellknown", "key": "k", "token": "t" },
        "some-future-provider": { "type": "quantum-handshake", "secret": "s" },
        "openai": { "type": "api", "key": "sk-old-0001", "metadata": { "label": "work" } },
    });
    fs::write(
        &store.path,
        serde_json::to_vec_pretty(&original).expect("the fixture serializes"),
    )
    .expect("the fixture writes");
    #[cfg(unix)]
    fs::set_permissions(&store.path, fs::Permissions::from_mode(0o600))
        .expect("the fixture is made private");

    // Neither entry is a usable credential, so neither is offered.
    assert_eq!(
        key_of(store.get("anthropic").expect("the file reads")),
        None
    );
    assert_eq!(
        store.stored().expect("the listing reads"),
        vec![(
            "openai".to_owned(),
            RedactedTail::of("sk-old-0001"),
            CredentialKind::ApiKey
        )]
    );

    store
        .set("anthropic", CANARY)
        .expect("a new key stores beside them");

    let rewritten: serde_json::Value =
        serde_json::from_slice(&fs::read(&store.path).expect("the file exists"))
            .expect("the file is still JSON");

    assert_eq!(
        rewritten["some-future-provider"],
        original["some-future-provider"]
    );
    assert_eq!(rewritten["openai"], original["openai"]);
    assert_eq!(rewritten["anthropic"]["type"], "api");
    assert_eq!(
        key_of(store.get("anthropic").expect("the new key reads back")),
        Some(CANARY.to_owned())
    );
}

/// The record upstream writes is `{type, access, refresh, expires,
/// ...extra}` (`provider/auth.ts:211-220`), and `...extra` is whatever the
/// login method returned — `accountId` from Codex, `enterpriseUrl` from
/// Copilot, and anything a plugin nobody has written yet decides to keep.
/// Reading one has to bring all of it back, and storing it again has to put
/// all of it down, or ganja is the tool that quietly deleted somebody's
/// account id.
#[test]
fn an_oauth_entry_round_trips_with_everything_it_arrived_with() {
    let directory = temporary();
    let store = store(&directory);
    let original = serde_json::json!({
        "type": "oauth",
        "refresh": "gho_refresh_0001",
        "access": "gho_access_0002",
        "expires": 1_785_000_000_000_u64,
        "accountId": "acct-42",
        "enterpriseUrl": "https://company.ghe.com",
        "someFuturePluginField": { "nested": [1, 2, 3] },
    });
    fs::write(
        &store.path,
        serde_json::to_vec_pretty(&serde_json::json!({ "github-copilot": original }))
            .expect("the fixture serializes"),
    )
    .expect("the fixture writes");
    #[cfg(unix)]
    fs::set_permissions(&store.path, fs::Permissions::from_mode(0o600))
        .expect("the fixture is made private");

    let credential = store
        .oauth("github-copilot")
        .expect("the file reads")
        .expect("the entry is an OAuth credential");

    assert_eq!(credential.refresh.expose_secret(), "gho_refresh_0001");
    assert_eq!(credential.access.expose_secret(), "gho_access_0002");
    assert_eq!(credential.expires, 1_785_000_000_000);
    assert_eq!(credential.account_id.as_deref(), Some("acct-42"));
    assert_eq!(
        credential.enterprise_url.as_deref(),
        Some("https://company.ghe.com")
    );
    assert_eq!(
        credential.extra.get("someFuturePluginField"),
        Some(&original["someFuturePluginField"]),
        "a field this build does not model has to survive the decode"
    );

    store
        .set_oauth("github-copilot", &credential)
        .expect("the credential stores again");

    let rewritten: serde_json::Value =
        serde_json::from_slice(&fs::read(&store.path).expect("the file exists"))
            .expect("the file is still JSON");
    assert_eq!(
        rewritten["github-copilot"], original,
        "storing what was read has to put back what was there"
    );
}

/// An OAuth credential is a credential, so it is listed as one — and as an
/// OAuth one, because "the key is ****0002" is a lie about a token that
/// expires.
#[test]
fn an_oauth_credential_is_listed_as_the_kind_it_is() {
    let directory = temporary();
    let store = store(&directory);
    store
        .set_oauth(
            "github-copilot",
            &OauthCredential::new(
                SecretString::from("gho_refresh_0001"),
                SecretString::from("gho_access_0002"),
                0,
            ),
        )
        .expect("the credential stores");
    store.set("openai", CANARY).expect("the key stores");

    assert_eq!(
        store.stored().expect("the listing reads"),
        vec![
            (
                "github-copilot".to_owned(),
                RedactedTail::of("gho_access_0002"),
                CredentialKind::Oauth
            ),
            (
                "openai".to_owned(),
                RedactedTail::of(CANARY),
                CredentialKind::ApiKey
            ),
        ]
    );
    // An OAuth credential is not an API key, and offering it as one would
    // send a bearer token out in an `x-api-key` header.
    assert_eq!(
        key_of(store.get("github-copilot").expect("the file reads")),
        None
    );
}

/// An entry with no token in it is not a credential, the same way an `api`
/// entry with an empty key is not one.
#[test]
fn an_oauth_entry_with_no_tokens_is_not_a_credential() {
    let directory = temporary();
    let store = store(&directory);
    store
        .set_oauth(
            "github-copilot",
            &OauthCredential::new(SecretString::from("  "), SecretString::from(""), 0),
        )
        .expect("the entry stores");

    assert!(
        store
            .oauth("github-copilot")
            .expect("the file reads")
            .is_none()
    );
    assert!(store.stored().expect("the listing reads").is_empty());
}

/// Copilot's credential never expires (`copilot.ts:294` stores `expires:
/// 0`), and reading that as "expired in 1970" would have every request
/// renewing a token that has no renewal endpoint.
///
/// This is the *stored deadline* alone, which is the narrow question
/// [`OauthCredential::needs_refresh`] answers. Whether a zero means what it
/// means here is `a_zero_expiry_is_copilots_promise_and_xais_silence`'s, and
/// the renewal decision that reads both is
/// `a_tokens_own_expiry_decides_a_renewal_the_stored_one_cannot`'s.
#[test]
fn a_credential_is_due_only_before_the_moment_it_expires() {
    let never = OauthCredential::new(SecretString::from("r"), SecretString::from("a"), 0);
    assert!(!never.needs_refresh(1_785_000_000_000, REFRESH_SKEW_MS));

    let expires_at = 1_785_000_000_000;
    let credential =
        OauthCredential::new(SecretString::from("r"), SecretString::from("a"), expires_at);

    assert!(!credential.needs_refresh(expires_at - REFRESH_SKEW_MS - 1, REFRESH_SKEW_MS));
    assert!(
        credential.needs_refresh(expires_at - REFRESH_SKEW_MS, REFRESH_SKEW_MS),
        "the margin is the point: a request started here would outlive the token"
    );
    assert!(!credential.needs_refresh(expires_at - 1, 0));
    assert!(credential.needs_refresh(expires_at, 0));

    // The clock is real, so this only pins the direction: an expiry in the
    // past is due and one a day out is not.
    let now = now_ms();
    assert!(
        OauthCredential::new(SecretString::from("r"), SecretString::from("a"), 1)
            .needs_refresh(now, 0)
    );
    assert!(
        !OauthCredential::new(
            SecretString::from("r"),
            SecretString::from("a"),
            now + 86_400_000
        )
        .needs_refresh(now, REFRESH_SKEW_MS)
    );
}

/// Two providers write a zero into the same field and mean opposite things
/// by it, so the field alone cannot answer the question and the provider
/// has to.
///
/// Collapsing the two readings back into one — whichever one — reddens this
/// test, which is the point of it: the bug it guards against is not a wrong
/// answer but a single answer.
#[test]
fn a_zero_expiry_is_copilots_promise_and_xais_silence() {
    assert_eq!(zero_expiry("github-copilot"), ZeroExpiry::Never);
    assert_eq!(zero_expiry("grok"), ZeroExpiry::Unrecorded);
    assert_eq!(
        zero_expiry("xai"),
        ZeroExpiry::Unrecorded,
        "the file's name for a provider and ganja's must not disagree about \
             the same credential"
    );
    assert_eq!(
        zero_expiry("some-provider-nobody-has-written-yet"),
        ZeroExpiry::Unrecorded,
        "a deadline nobody wrote down is not a deadline nobody has"
    );

    // One credential, byte for byte, read by two providers' rules: no
    // stored deadline, and an access token whose own is exactly now.
    let same_bytes = credential(jwt(NOW_S - 3_600, NOW_S), 0);

    assert!(
        !same_bytes.needs_refresh_for("github-copilot", NOW_MS, REFRESH_SKEW_MS),
        "Copilot's zero is a promise that it never expires (`copilot.ts:294`), \
             and there is no renewal endpoint to send a due credential to"
    );
    assert!(
        same_bytes.needs_refresh_for("grok", NOW_MS, REFRESH_SKEW_MS),
        "xAI's zero is a deadline nobody recorded (`xai.ts:491`), so the \
             token's own is what decides"
    );
}

/// Upstream decodes the access token's own `exp` and treats it as the
/// deadline for a credential whose stored one says nothing
/// (`xai.ts:95-116`, and `:485-490` for why: "the JWT check is the
/// load-bearing one for tokens that lack a fresh stored deadline").
#[test]
fn a_tokens_own_expiry_decides_a_renewal_the_stored_one_cannot() {
    // Issued an hour ago, a minute of life left.
    assert!(
        credential(jwt(NOW_S - 3_540, NOW_S + 60), 0).needs_refresh_for(
            "grok",
            NOW_MS,
            REFRESH_SKEW_MS
        ),
        "a minute left is inside the two-minute margin one long tool call needs"
    );
    // Issued this second, good for a day. Its `iat` and `nbf` are both
    // already past, so a decode reading either instead of `exp` calls this
    // spent and this assertion is what says so.
    assert!(
        !credential(jwt(NOW_S, NOW_S + 86_400), 0).needs_refresh_for(
            "grok",
            NOW_MS,
            REFRESH_SKEW_MS
        ),
        "a token good for a day has said so, and spending a rotating refresh \
             token on it costs a round trip for nothing"
    );

    // Everything that is not a JWT carrying an `exp` contributes nothing,
    // which leaves the stored deadline — here, absent — in charge.
    for opaque in [
        "at-opaque-nothing-to-decode",
        "two.segments",
        "four.of.these.things",
        "eyJhbGciOiJSUzI1NiJ9.!!!not-base64!!!.sig",
    ] {
        assert!(
            !credential(SecretString::from(opaque), 0).needs_refresh_for(
                "grok",
                NOW_MS,
                REFRESH_SKEW_MS
            ),
            "{opaque} is not a token with a deadline in it"
        );
    }

    let no_exp = {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let payload = URL_SAFE_NO_PAD.encode(r#"{"sub":"someone"}"#);
        SecretString::from(format!("eyJhbGciOiJSUzI1NiJ9.{payload}.sig"))
    };
    assert!(
        !credential(no_exp, 0).needs_refresh_for("grok", NOW_MS, REFRESH_SKEW_MS),
        "a JWT that names no `exp` names no deadline"
    );
}

/// The `exp` inside an access token is a reason to renew and never a reason
/// to refuse. Nobody checked the signature, so a forged claim must not be
/// able to make a credential the store calls live unusable.
#[test]
fn a_tokens_own_expiry_never_decides_whether_it_may_be_sent() {
    // A token that says it died yesterday, stored with a deadline a day out
    // — which is exactly the disagreement a forged claim would manufacture.
    let credential = credential(jwt(NOW_S - 90_000, NOW_S - 86_400), NOW_MS + 86_400_000);

    assert!(
        credential.needs_refresh_for("grok", NOW_MS, REFRESH_SKEW_MS),
        "the token says it is spent, which is a reason to renew it early"
    );
    assert!(
        !credential.needs_refresh(NOW_MS, 0),
        "and never a reason to call spent what the store calls live"
    );
    assert!(
        !credential.needs_refresh(NOW_MS, REFRESH_SKEW_MS),
        "the stored-deadline predicate is deliberately blind to the claim"
    );
}

/// A caller that cannot get a usable credential is told which of the four
/// situations it is in, and what fixes it.
///
/// Four since P22 (`flp`) and four before it: the retired `Expired` row
/// was the fifth error and the fourth *kind*, because a spent-but-renewable
/// token is not an outcome any caller is handed — [`Refresher::usable`]
/// renews it and reports only what came back.
#[test]
fn every_failure_says_which_of_them_it_is_and_what_to_do() {
    #[cfg(not(windows))]
    let permissions = (
        AuthError::Permissions {
            path: PathBuf::from("/tmp/auth.json"),
            mode: 0o644,
        },
        AuthErrorKind::Storage,
        "chmod 600",
    );
    #[cfg(windows)]
    let permissions = (
        AuthError::Permissions {
            path: PathBuf::from(r"C:\temp\auth.json"),
            grantee: "Everyone".to_owned(),
        },
        AuthErrorKind::Storage,
        "icacls",
    );
    let taxonomy = [
        (
            AuthError::NotOauth {
                provider_id: "openai".to_owned(),
                found: "an API key is stored",
            },
            AuthErrorKind::NotOauth,
            "ganja auth login openai",
        ),
        (
            AuthError::ReauthRequired {
                provider_id: "openai".to_owned(),
                reason: "HTTP 400 invalid_grant".to_owned(),
            },
            AuthErrorKind::ReauthRequired,
            "ganja auth login openai",
        ),
        (
            AuthError::RefreshUnavailable {
                provider_id: "openai".to_owned(),
                reason: "the endpoint could not be reached".to_owned(),
            },
            AuthErrorKind::RefreshUnavailable,
            "try again",
        ),
        permissions,
    ];

    for (error, kind, remedy) in taxonomy {
        assert_eq!(error.kind(), kind, "{error}");
        assert!(
            error.to_string().contains(remedy),
            "an error has to say what the caller can do about it: {error}"
        );
    }
}

/// `auth.json` is shared territory, so the key is upstream's name for the
/// provider even where ganja's own is different.
#[test]
fn a_grok_credential_is_stored_where_upstream_keeps_its_xai_one() {
    let directory = temporary();
    let store = store(&directory);
    store.set("grok", CANARY).expect("the key stores");

    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(&store.path).expect("the file exists"))
            .expect("the file is JSON");
    assert_eq!(written["xai"]["key"], CANARY);
    assert!(
        written.get("grok").is_none(),
        "a second key for the same account is how a login gets lost: {written}"
    );

    // Either name reaches it, so a caller that has upstream's does not have
    // to know about ganja's.
    assert_eq!(
        key_of(store.get("grok").expect("the file reads")),
        Some(CANARY.to_owned())
    );
    assert_eq!(
        key_of(store.get("xai").expect("the file reads")),
        Some(CANARY.to_owned())
    );
    assert!(store.remove("grok").expect("the key is removable"));

    assert_eq!(storage_key("grok"), "xai");
    assert_eq!(storage_key("openai"), "openai");
    assert_eq!(provider_id_for_storage_key("xai"), "grok");
    assert_eq!(provider_id_for_storage_key("openai"), "openai");

    // The alias table is a **closed** list over an **open** store: a name
    // it has never heard of passes through unchanged in both directions.
    // That is what lets a config declare a provider and
    // `ganja auth login <id>` write exactly where selection reads — a
    // translation applied to an unknown id would file the credential under
    // a name nothing looks for.
    for configured in ["local-llama", "gateway", "cursor"] {
        assert_eq!(storage_key(configured), configured);
        assert_eq!(provider_id_for_storage_key(configured), configured);
    }
    store
        .set("local-llama", CANARY)
        .expect("an id nothing ships is still an id");
    assert_eq!(
        key_of(store.get("local-llama").expect("the file reads")),
        Some(CANARY.to_owned()),
        "a configured provider's key is read back under the name it was written under"
    );
}

#[test]
fn an_entry_storing_an_empty_key_is_not_a_credential() {
    let directory = temporary();
    let store = store(&directory);
    store.set("openai", "   ").expect("the entry stores");

    assert_eq!(key_of(store.get("openai").expect("the file reads")), None);
    assert!(store.stored().expect("the listing reads").is_empty());
}

/// Corruption is reported rather than read as "no credentials", which
/// would send someone hunting for a key that is sitting right there. The
/// report itself must not quote the file back, since the file is full of
/// secrets.
#[test]
fn a_file_that_is_not_a_json_object_is_reported_without_quoting_it_back() {
    let corrupt: [&[u8]; 3] = [
        b"{ this is not json",
        // A document whose shape is wrong, carrying a key: the parser sees
        // the secret, and still must not put it in the message.
        br#"["sk-canary-8842"]"#,
        br#""sk-canary-8842""#,
    ];

    for fixture in corrupt {
        let directory = temporary();
        let store = store(&directory);
        fs::write(&store.path, fixture).expect("the fixture writes");
        #[cfg(unix)]
        fs::set_permissions(&store.path, fs::Permissions::from_mode(0o600))
            .expect("the fixture is made private");

        let error = store.get("anthropic").expect_err("corruption is reported");

        assert!(
            matches!(error, AuthError::Malformed { .. }),
            "got {error:?}"
        );
        for line in rendered(&error) {
            assert!(
                !line.contains(CANARY),
                "an error must not carry the file's contents: {line}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn a_written_store_is_private_to_its_owner() {
    let directory = temporary();
    let store = store(&directory);

    store.set("anthropic", CANARY).expect("the key stores");
    assert_eq!(mode(&store.path), 0o600);

    // A second write goes through the same rename dance and must not
    // loosen anything.
    store.set("openai", CANARY).expect("a second key stores");
    assert_eq!(mode(&store.path), 0o600);

    // An OAuth credential is two more secrets in the same file, written
    // through the same `write`; it must not be the one that widens it.
    store
        .set_oauth(
            "github-copilot",
            &OauthCredential::new(
                SecretString::from("gho_refresh_0001"),
                SecretString::from("gho_access_0002"),
                0,
            ),
        )
        .expect("the credential stores");
    assert_eq!(mode(&store.path), 0o600);
    assert!(
        fs::read_dir(directory.path())
            .expect("the directory lists")
            .filter_map(Result::ok)
            .all(|entry| {
                entry.file_name() == "auth.json" || entry.file_name() == "auth-stamps.json"
            }),
        "no temporary file may outlive a write, the stamps' included"
    );
    assert_eq!(
        mode(&store.stamps_path()),
        0o600,
        "the sidecar holds no secret, but it lives in the store's directory \
             and gets the store's posture"
    );
}

/// The store is written through a temporary file whose name is derived from
/// the process id, so anyone else on the machine can work out what it will
/// be called and plant a symbolic link there first. An open that followed
/// it would write every stored key wherever the link led — and then rename
/// the link itself over `auth.json`, leaving the store pointing at it. The
/// open is exclusive, so it refuses the name instead of following it.
#[cfg(unix)]
#[test]
fn a_link_planted_at_the_temporary_file_cannot_redirect_the_write() {
    let directory = temporary();
    let store = store(&directory);

    let target = directory.path().join("somewhere-else");
    fs::write(&target, b"not a credential store").expect("the target writes");
    let planted = directory
        .path()
        .join(format!("auth.json.{}.tmp", std::process::id()));
    std::os::unix::fs::symlink(&target, &planted).expect("the link plants");

    store
        .set("anthropic", CANARY)
        .expect("the key still stores");

    assert_eq!(
        fs::read_to_string(&target).expect("the target still exists"),
        "not a credential store",
        "the write followed a planted link"
    );
    assert_eq!(
        key_of(store.get("anthropic").expect("the store reads back")),
        Some(CANARY.to_owned()),
        "refusing the planted name must not cost the write"
    );
    assert!(
        !planted.is_symlink(),
        "the temporary file should not outlive the write"
    );
    assert!(
        !fs::read_to_string(&store.path)
            .expect("the store exists")
            .is_empty(),
        "the store should hold the key, not be a link to somewhere else"
    );
}

/// The other half of creating the temporary file exclusively: a write that
/// died between creating it and renaming it leaves the name behind, and a
/// build that only ever refused an existing name would then never be able
/// to store a key again until someone deleted a file they have no reason to
/// know about. The name is removed and re-created, so a crash costs
/// nothing.
#[test]
fn a_temporary_file_left_by_a_crashed_write_does_not_wedge_the_store() {
    let directory = temporary();
    let store = store(&directory);
    let stale = directory
        .path()
        .join(format!("auth.json.{}.tmp", std::process::id()));
    fs::write(&stale, b"{ half a write that never landed").expect("the stale file writes");

    store
        .set("anthropic", CANARY)
        .expect("the key still stores");

    assert_eq!(
        key_of(store.get("anthropic").expect("the store reads back")),
        Some(CANARY.to_owned())
    );
    assert!(!stale.exists(), "the stale file should have been consumed");
}

/// A key readable by other users of the machine is already compromised;
/// reading it anyway would hide that.
#[cfg(unix)]
#[test]
fn a_group_or_world_readable_store_is_refused_with_a_way_out() {
    for exposed in [0o640, 0o604, 0o660, 0o666] {
        let directory = temporary();
        let store = store(&directory);
        store.set("anthropic", CANARY).expect("the key stores");
        fs::set_permissions(&store.path, fs::Permissions::from_mode(exposed))
            .expect("the mode is loosened");

        let error = store.get("anthropic").expect_err("exposure is refused");
        let explanation = error.to_string();

        assert!(
            matches!(error, AuthError::Permissions { mode, .. } if mode == exposed),
            "{exposed:04o} should be refused, got {error:?}"
        );
        assert!(
            explanation.contains("chmod 600"),
            "the way out should be spelled out: {explanation}"
        );
        for line in rendered(&error) {
            assert!(
                !line.contains(CANARY),
                "an error must not carry the key: {line}"
            );
        }
    }
}

/// `ganja auth list` prints these in fixed-width columns. A `Display`
/// written with `write_str` accepts a width and then silently drops it,
/// which lines the header up with nothing and lines `api` rows up with
/// `oauth` rows only by accident — the two words are not the same length.
#[test]
fn a_listed_column_is_as_wide_as_it_was_asked_to_be() {
    assert_eq!(format!("{:<5}|", CredentialKind::ApiKey), "api  |");
    assert_eq!(format!("{:<5}|", CredentialKind::Oauth), "oauth|");
    assert_eq!(
        format!("{:<9}|", RedactedTail::of("sk-test-ABCD")),
        "****ABCD |",
    );
}

#[test]
fn nothing_renders_a_whole_key() {
    let credential = Credential {
        api_key: SecretString::from(CANARY),
    };
    let tail = credential.tail();
    let entry = Entry {
        provider_id: "anthropic".to_owned(),
        tail: tail.clone(),
        source: Source::Environment("ANTHROPIC_API_KEY"),
        kind: CredentialKind::ApiKey,
        shadowed_by: None,
    };

    let renderings = [
        format!("{credential:?}"),
        format!("{credential}"),
        format!("{tail:?}"),
        format!("{tail}"),
        format!("{entry:?}"),
        tail.as_str().to_owned(),
    ];

    for rendering in &renderings {
        assert!(
            !rendering.contains(CANARY) && !rendering.contains("sk-canary"),
            "a whole key reached output: {rendering}"
        );
        assert!(
            rendering.contains("8842"),
            "the tail is what identifies a key: {rendering}"
        );
    }

    assert_eq!(tail.as_str(), "****8842");
    // A key too short to have a tail still shows nothing of itself.
    assert_eq!(RedactedTail::of("ab").as_str(), "****ab");
    assert_eq!(RedactedTail::of("").as_str(), "****");

    // The field is public, so something that renders it directly rather
    // than going through `Credential` has to be redacted too.
    let field = format!("{:?}", credential.api_key);
    assert!(
        !field.contains(CANARY) && field.contains("REDACTED"),
        "the key material renders itself: {field}"
    );
}

/// Same rule for an OAuth credential, and one place more to leak from: the
/// unmodelled extras are values this build has never seen, and a plugin
/// keeping its own token in one is exactly the case a derived `Debug` would
/// print.
#[test]
fn nothing_renders_a_whole_token_including_the_fields_this_build_cannot_read() {
    let mut credential = OauthCredential::new(
        SecretString::from(format!("refresh-{CANARY}")),
        SecretString::from(CANARY),
        0,
    );
    credential.account_id = Some("acct-42".to_owned());
    credential.extra.insert(
        "somePluginToken".to_owned(),
        serde_json::Value::from(CANARY),
    );

    let renderings = [
        format!("{credential:?}"),
        format!("{credential}"),
        format!("{:?}", super::Stored::Oauth(credential.clone())),
        credential.tail().as_str().to_owned(),
    ];

    for rendering in &renderings {
        assert!(
            !rendering.contains(CANARY) && !rendering.contains("sk-canary"),
            "a whole token reached output: {rendering}"
        );
        assert!(
            rendering.contains("8842"),
            "the tail is what identifies a token: {rendering}"
        );
    }
    assert!(
        format!("{credential:?}").contains("somePluginToken"),
        "the names of the unread fields are worth showing; their values are not"
    );
}

#[test]
fn every_shipped_provider_has_a_key_variable_and_others_do_not() {
    assert_eq!(key_var("anthropic"), Some("ANTHROPIC_API_KEY"));
    assert_eq!(key_var("openai"), Some("OPENAI_API_KEY"));
    assert_eq!(key_var("fake"), None);
}

/// The whole precedence chain, against the real XDG resolution: nothing,
/// then a stored key, then an environment variable that outranks it, then
/// an empty variable that does not.
#[test]
fn the_environment_outranks_the_file_and_an_empty_variable_outranks_nothing() {
    let _guard = environment();
    let directory = temporary();

    clear_keys();
    set_env("XDG_DATA_HOME", Some(&directory.path().to_string_lossy()));

    let expected = directory.path().join("ganja").join("auth.json");
    assert_eq!(store_path().expect("the path resolves"), expected);

    assert_eq!(
        key_of(credential_for("anthropic").expect("an empty environment is fine")),
        None
    );
    assert!(list_providers().expect("the listing reads").is_empty());

    set_credential("anthropic", "sk-stored-0001").expect("the key stores");
    assert!(expected.is_file(), "the parent directories are created");
    assert_eq!(
        key_of(credential_for("anthropic").expect("the stored key reads")),
        Some("sk-stored-0001".to_owned())
    );
    assert_eq!(
        list_providers().expect("the listing reads"),
        vec![Entry {
            provider_id: "anthropic".to_owned(),
            tail: RedactedTail::of("sk-stored-0001"),
            source: Source::File,
            kind: CredentialKind::ApiKey,
            shadowed_by: None,
        }]
    );

    set_env("ANTHROPIC_API_KEY", Some(CANARY));
    assert_eq!(
        key_of(credential_for("anthropic").expect("the environment reads")),
        Some(CANARY.to_owned()),
        "the environment has to win"
    );
    assert_eq!(
        list_providers()
            .expect("the listing reads")
            .first()
            .map(|entry| entry.source),
        Some(Source::Environment("ANTHROPIC_API_KEY")),
        "the listing has to show the credential actually in use"
    );

    // Whitespace around a key pasted out of a file is trimmed, and an
    // exported-but-empty variable falls through to the stored key.
    set_env("ANTHROPIC_API_KEY", Some("  sk-padded-0002\n"));
    assert_eq!(
        key_of(credential_for("anthropic").expect("the environment reads")),
        Some("sk-padded-0002".to_owned())
    );

    set_env("ANTHROPIC_API_KEY", Some("   "));
    assert_eq!(
        key_of(credential_for("anthropic").expect("the stored key reads")),
        Some("sk-stored-0001".to_owned()),
        "an empty variable must not shadow a stored key"
    );

    clear_keys();
    set_env("XDG_DATA_HOME", None);
}

/// The newest key provider gets the same precedence the two older ones
/// have, proved rather than assumed.
///
/// It is one row in [`KEY_VARS`] and one line in [`STORAGE_ALIASES`] that
/// it is deliberately *not* in, and both are easy to get wrong in ways
/// nothing else would notice: a missing row makes an exported key invisible
/// (the stored one answers, silently, for the whole session), and an alias
/// nobody needed would file the credential where an opencode install
/// reading the same file could not find it.
#[test]
fn an_exported_gateway_key_outranks_a_stored_one_and_is_filed_under_its_own_name() {
    let _guard = environment();
    let directory = temporary();

    clear_keys();
    set_env("XDG_DATA_HOME", Some(&directory.path().to_string_lossy()));

    let provider = crate::provider::openrouter::ID;
    let variable = crate::provider::openrouter::API_KEY_ENV;
    assert_eq!(key_var(provider), Some(variable));

    set_credential(provider, "sk-or-stored-0001").expect("the key stores");
    assert_eq!(
        key_of(credential_for(provider).expect("the stored key reads")),
        Some("sk-or-stored-0001".to_owned())
    );

    // Under its own name on disk, which is upstream's name too — a gateway
    // whose credential ganja filed somewhere else would be a login opencode
    // could not see and `config import-opencode` could not translate.
    let stored: std::collections::BTreeMap<String, serde_json::Value> = serde_json::from_slice(
        &fs::read(store_path().expect("the store has a path")).expect("the store exists"),
    )
    .expect("the store is JSON");
    assert!(
        stored.contains_key(provider),
        "the entry is filed under {provider}: {stored:?}"
    );

    set_env(variable, Some(CANARY));
    assert_eq!(
        key_of(credential_for(provider).expect("the environment reads")),
        Some(CANARY.to_owned()),
        "the environment has to win here too"
    );
    assert_eq!(
        list_providers()
            .expect("the listing reads")
            .iter()
            .find(|entry| entry.provider_id == provider)
            .map(|entry| entry.source),
        Some(Source::Environment("OPENROUTER_API_KEY")),
        "the listing has to show the credential actually in use"
    );

    // And an exported-but-empty variable falls through rather than
    // shadowing, which is the half that decides whether a session with a
    // blank export dies at startup or quietly runs on the stored key.
    set_env(variable, Some("   "));
    assert_eq!(
        key_of(credential_for(provider).expect("the stored key reads")),
        Some("sk-or-stored-0001".to_owned()),
        "an empty variable must not shadow a stored key"
    );

    clear_keys();
    set_env("XDG_DATA_HOME", None);
}

/// **Two providers, one variable, two stores** — the gateway pair's whole
/// credential arrangement, and every part of it is a thing that could
/// plausibly have been done the other way.
///
/// The variable is shared because the catalog names it in the `env` of both
/// and one key was measured answering on both hosts. The *storage* is not,
/// because the vendor's own client keys credentials by provider id — so a
/// key stored for Zen must not silently answer for Go, or logging out of
/// one would leave the other working and nobody could say why.
#[test]
fn the_gateway_pair_shares_a_variable_and_shares_no_stored_credential() {
    let _guard = environment();
    let directory = temporary();

    clear_keys();
    set_env("XDG_DATA_HOME", Some(&directory.path().to_string_lossy()));

    let zen = crate::provider::opencode::ZEN_ID;
    let go = crate::provider::opencode::GO_ID;
    let variable = crate::provider::opencode::API_KEY_ENV;

    assert_eq!(key_var(zen), Some(variable), "one variable…");
    assert_eq!(key_var(go), Some(variable), "…named by both");
    assert_eq!(storage_key(zen), zen, "and stored under their own ids,");
    assert_eq!(storage_key(go), go, "with no alias between them");

    // Stored for one is not stored for the other.
    set_credential(zen, "sk-zen-stored-0001").expect("the key stores");
    assert_eq!(
        key_of(credential_for(zen).expect("the stored key reads")),
        Some("sk-zen-stored-0001".to_owned())
    );
    assert_eq!(
        key_of(credential_for(go).expect("an empty slot is fine")),
        None,
        "a Zen login is not a Go login: the vendor keys them apart and so \
             does this"
    );

    // The one export answers for both, which is what makes the shared
    // variable worth having.
    set_env(variable, Some(CANARY));
    for id in [zen, go] {
        assert_eq!(
            key_of(credential_for(id).expect("the environment reads")),
            Some(CANARY.to_owned()),
            "{id} reads the shared variable"
        );
    }

    set_env(variable, Some("   "));
    assert_eq!(
        key_of(credential_for(zen).expect("the stored key reads")),
        Some("sk-zen-stored-0001".to_owned()),
        "an empty variable must not shadow a stored key"
    );
    assert_eq!(
        key_of(credential_for(go).expect("an empty slot is fine")),
        None,
        "and must not conjure one that was never stored"
    );

    clear_keys();
    set_env("XDG_DATA_HOME", None);
}

/// A login somebody has just completed has to be visible to the command
/// whose whole job is saying what credentials there are, even when a
/// variable outranks it.
///
/// The shape measured live: a ChatGPT login stored under `openai` while
/// `OPENAI_API_KEY` was exported, and a listing that printed only the
/// variable — so the login looked as though it had never landed. Both rows
/// now, the winner first, and the loser saying what beat it.
#[test]
fn a_stored_login_stays_in_the_listing_when_a_variable_outranks_it() {
    let _guard = environment();
    let directory = temporary();

    clear_keys();
    set_env("XDG_DATA_HOME", Some(&directory.path().to_string_lossy()));

    set_oauth(
        "openai",
        &OauthCredential::new(
            SecretString::from("rt-listing-0001"),
            SecretString::from("at-listing-0002"),
            NOW_MS,
        ),
    )
    .expect("the login stores");
    set_env("OPENAI_API_KEY", Some(CANARY));

    let listed = list_providers().expect("the listing reads");
    assert_eq!(
        listed
            .iter()
            .map(|entry| (entry.source, entry.kind, entry.shadowed_by))
            .collect::<Vec<_>>(),
        vec![
            (
                Source::Environment("OPENAI_API_KEY"),
                CredentialKind::ApiKey,
                None
            ),
            (Source::File, CredentialKind::Oauth, Some("OPENAI_API_KEY")),
        ],
        "the credential in use comes first and the one it outranks says so"
    );
    assert_eq!(
        listed
            .iter()
            .map(|entry| entry.tail.clone())
            .collect::<Vec<_>>(),
        vec![
            RedactedTail::of(CANARY),
            RedactedTail::of("at-listing-0002")
        ],
        "each row shows its own credential rather than the winner's twice"
    );
    // And the precedence the listing is describing is still the one that
    // decides a request: this reports, it does not choose.
    assert_eq!(
        key_of(credential_for("openai").expect("the environment reads")),
        Some(CANARY.to_owned()),
    );

    // A provider with nothing exported keeps its single unshadowed row,
    // which is what makes the marker a statement rather than decoration.
    set_credential("anthropic", "sk-stored-0003").expect("the key stores");
    assert_eq!(
        list_providers()
            .expect("the listing reads")
            .iter()
            .find(|entry| entry.provider_id == "anthropic")
            .and_then(|entry| entry.shadowed_by),
        None
    );

    clear_keys();
    set_env("XDG_DATA_HOME", None);
}

/// A login's stamp is what lets a session default to the oldest one, so
/// both login paths mint it — and a credential replaced in place keeps the
/// one it had, because the seniority is the login's, not the write's.
#[test]
fn a_login_is_stamped_when_it_lands_and_a_replacement_keeps_its_seniority() {
    let directory = temporary();
    let store = store(&directory);

    let before = now_ms();
    store.set("anthropic", CANARY).expect("the key stores");
    let after = now_ms();

    let minted = store.read_stamps()["anthropic"];
    assert!(
        (before..=after).contains(&minted),
        "a login is stamped with the moment it landed: {minted} not in {before}..={after}"
    );

    // An aged stamp, then the same provider stored again.
    fs::write(store.stamps_path(), r#"{"anthropic": 1000}"#).expect("the stamps rewrite");
    store
        .set("anthropic", "sk-rotated-0002")
        .expect("the key stores again");
    assert_eq!(store.read_stamps()["anthropic"], 1000);

    store
        .set_oauth(
            "github-copilot",
            &OauthCredential::new(
                SecretString::from("gho_refresh_0001"),
                SecretString::from("gho_access_0002"),
                0,
            ),
        )
        .expect("the login stores");
    assert!(
        store.read_stamps().contains_key("github-copilot"),
        "an OAuth login is as much a login as a key, and stamps the same way"
    );
}

/// The order selection defaults through when nothing named a provider:
/// oldest stamp first, then the logins nothing stamped in the fixed
/// priority — ganja's `grok` under the file's `xai` included — then ids
/// the priority has never heard of, in the store's own order.
#[test]
fn the_oldest_stamped_login_leads_and_the_unstamped_follow_in_fixed_priority() {
    let directory = temporary();
    let store = store(&directory);
    for provider_id in [
        "local-llama",
        "github-copilot",
        "grok",
        "openai",
        "anthropic",
    ] {
        store.set(provider_id, CANARY).expect("the key stores");
    }

    // Everybody unstamped — the pre-feature store, and opencode's forever.
    fs::write(store.stamps_path(), "{}").expect("the stamps clear");
    assert_eq!(
        store.logins_oldest_first().expect("the store reads"),
        vec![
            "anthropic",
            "openai",
            "xai",
            "github-copilot",
            "local-llama"
        ],
    );

    // One stamp, held by the login the fixed priority ranks last: a
    // recorded age beats every guessed one.
    fs::write(store.stamps_path(), r#"{"github-copilot": 5000}"#).expect("the stamps rewrite");
    assert_eq!(
        store.logins_oldest_first().expect("the store reads"),
        vec![
            "github-copilot",
            "anthropic",
            "openai",
            "xai",
            "local-llama"
        ],
    );

    // Two stamps order by time, not by name or by priority.
    fs::write(store.stamps_path(), r#"{"anthropic": 9000, "xai": 2000}"#)
        .expect("the stamps rewrite");
    assert_eq!(
        store.logins_oldest_first().expect("the store reads"),
        vec![
            "xai",
            "anthropic",
            "openai",
            "github-copilot",
            "local-llama"
        ],
    );
}

/// A logout drops the stamp with the credential — logging in again later
/// is a new login — and a stamp orphaned by a tool that does not know the
/// sidecar exists is pruned at the next write rather than left to vote
/// for a login that is gone.
#[test]
fn a_logout_ends_a_logins_seniority_and_an_orphaned_stamp_is_pruned() {
    let directory = temporary();
    let store = store(&directory);
    store.set("anthropic", CANARY).expect("the key stores");
    store.set("openai", CANARY).expect("the key stores");
    fs::write(
        store.stamps_path(),
        r#"{"anthropic": 1000, "openai": 2000}"#,
    )
    .expect("the stamps rewrite");

    assert!(store.remove("anthropic").expect("the key is removable"));
    assert_eq!(
        store.read_stamps(),
        std::collections::BTreeMap::from([("openai".to_owned(), 2000)])
    );

    store
        .set("anthropic", CANARY)
        .expect("the key stores again");
    assert!(
        store.read_stamps()["anthropic"] > 2000,
        "a login after a logout starts its seniority over"
    );

    // Opencode's `Auth.remove` rewrites `auth.json` and nothing else, so
    // the stamp it orphans is this build's to notice.
    fs::write(store.stamps_path(), r#"{"anthropic": 1000, "gemini": 500}"#)
        .expect("the stamps rewrite");
    store.set("openai", CANARY).expect("the key stores again");
    let pruned = store.read_stamps();
    assert!(
        !pruned.contains_key("gemini"),
        "a stamp with no credential under it is not a login: {pruned:?}"
    );
    assert_eq!(
        pruned["anthropic"], 1000,
        "the live stamps survive the prune"
    );
}

/// A refresh rewrites the login it was given. Minting a stamp there would
/// walk a pre-feature credential into the stamped tier at whatever moment
/// its token happened to expire, and the oldest-login default would flip
/// under whoever was relying on it.
#[test]
fn a_renewal_is_not_a_login_and_mints_no_stamp() {
    let directory = temporary();
    let store = store(&directory);
    let credential = OauthCredential::new(
        SecretString::from("rt-renew-0001"),
        SecretString::from("at-renew-0002"),
        NOW_MS,
    );
    store
        .set_oauth("grok", &credential)
        .expect("the login stores");
    // The pre-feature shape: a credential on disk, no stamp anywhere.
    fs::write(store.stamps_path(), "{}").expect("the stamps clear");

    store
        .renew_oauth("grok", &credential)
        .expect("the renewal stores");
    assert!(
        store.read_stamps().is_empty(),
        "a renewal walked an unstamped login into the stamped tier"
    );

    // And a stamped login keeps exactly what it has.
    fs::write(store.stamps_path(), r#"{"xai": 1000}"#).expect("the stamps rewrite");
    store
        .renew_oauth("grok", &credential)
        .expect("the renewal stores");
    assert_eq!(store.read_stamps()["xai"], 1000);
}

/// The sidecar holds provider names and timestamps, no secrets, so a file
/// that is not what it should be degrades to the fixed order instead of
/// failing a startup the way corruption in the store itself must.
#[test]
fn a_broken_stamps_file_degrades_to_the_fixed_priority_order() {
    let directory = temporary();
    let store = store(&directory);
    store.set("openai", CANARY).expect("the key stores");
    store.set("anthropic", CANARY).expect("the key stores");
    fs::write(store.stamps_path(), b"{ not json").expect("the fixture writes");

    assert_eq!(
        store
            .logins_oldest_first()
            .expect("a broken sidecar is not a broken store"),
        vec!["anthropic".to_owned(), "openai".to_owned()],
    );
}

/// The whole reason the stamp is a sidecar: upstream's `Auth.set` rebuilds
/// every entry from its schema and rewrites the file (`auth/index.ts:66`,
/// `:79`), so anything ganja put inside an entry would die on opencode's
/// next write. Nothing of the stamps may land in `auth.json`.
#[test]
fn the_stamps_never_touch_the_shape_upstream_reads() {
    let directory = temporary();
    let store = store(&directory);
    store.set("openai", CANARY).expect("the key stores");

    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(&store.path).expect("the file exists"))
            .expect("the file is JSON");
    assert_eq!(
        written["openai"],
        serde_json::json!({"type": "api", "key": CANARY}),
        "the entry carries exactly the fields upstream's schema declares"
    );
    assert!(
        store.stamps_path().is_file(),
        "the stamp went beside the store, not into it"
    );
}
