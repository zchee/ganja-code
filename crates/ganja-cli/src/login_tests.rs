use ganja_provider::auth::copilot::Deployment;

use super::{
    DeploymentAnswer, DeploymentKind, Method, accepted, chosen, deployment, label, loopback_origin,
};
use crate::ProviderId;

/// Every pairing the CLI can be asked for, so a login that exists is never
/// refused and one that does not is never attempted.
#[test]
fn a_provider_accepts_exactly_the_logins_this_build_has_for_it() {
    assert_eq!(ProviderId::Anthropic.methods(), [Method::Api]);
    assert_eq!(
        ProviderId::Cursor.methods(),
        [Method::Browser],
        "the login landed ahead of the wire, and it is OAuth-only: a \
             stored key would be a credential nothing ever sends"
    );
    assert_eq!(ProviderId::OpenAi.methods(), [Method::Browser, Method::Device, Method::Api]);
    assert_eq!(ProviderId::Grok.methods(), [Method::Browser, Method::Device, Method::Api]);
    assert_eq!(ProviderId::GithubCopilot.methods(), [Method::Device, Method::Api]);
}

/// A menu is only worth drawing when there is something to choose, and
/// upstream skips it for the same reason (`providers.ts:47`).
#[test]
fn only_the_providers_with_more_than_one_login_are_asked_which() {
    assert_eq!(ProviderId::Anthropic.only_login(), Some(Method::Api));
    assert_eq!(ProviderId::GithubCopilot.only_login(), Some(Method::Device));
    assert_eq!(ProviderId::OpenAi.only_login(), None);
    assert_eq!(
        ProviderId::Grok.only_login(),
        None,
        "a browser login and a device login answer different questions about \
             this machine, and nothing here can tell which one somebody is in"
    );
    assert_eq!(
        ProviderId::Cursor.only_login(),
        Some(Method::Browser),
        "one login worth offering means no menu, which is what makes the \
             headless invocation reach it"
    );
}

/// The standing refusal is gone — deliberately: the login landed ahead of
/// its wire, because a stored credential is real value the day the wire
/// arrives. What is refused now is only what cursor does not have: a key,
/// in every spelling that could store one.
#[test]
fn a_cursor_login_runs_its_browser_flow_and_a_key_for_it_is_refused() {
    // Steps 3 and 5 of the ladder, exactly as Copilot's device grant
    // takes them: the one login runs, terminal or not, so nothing here
    // consults standard input.
    assert_eq!(
        chosen(ProviderId::Cursor, false, None).expect("cursor's one login is chosen unasked"),
        Method::Browser
    );
    assert_eq!(
        chosen(ProviderId::Cursor, false, Some(Method::Browser))
            .expect("naming the login cursor has is not refused"),
        Method::Browser
    );

    // `--key` and `--method api` are the shapes that would store what
    // nothing sends, and the refusal names what to use instead.
    for (has_key, method) in [(true, None), (false, Some(Method::Api))] {
        let refused = chosen(ProviderId::Cursor, has_key, method)
            .expect_err("cursor has no key to store")
            .to_string();
        assert!(refused.contains("no `api` login") && refused.contains("`browser`"), "{refused}");
    }

    let refused = chosen(ProviderId::Cursor, false, Some(Method::Device))
        .expect_err("cursor has no device grant")
        .to_string();
    assert!(refused.contains("no `device` login"), "{refused}");
}

/// The menu is what somebody without `--method` actually reads, so the words
/// on it are worth pinning — they are upstream's, which is what makes them
/// recognisable to anybody who arrived from its documentation.
#[test]
fn groks_menu_offers_upstreams_two_oauth_logins_by_upstreams_names() {
    let offered: Vec<String> =
        ProviderId::Grok.methods().iter().map(|method| label(ProviderId::Grok, *method)).collect();

    assert_eq!(
        offered,
        [
            "xAI Grok OAuth (SuperGrok Subscription)",
            "xAI Grok OAuth (Headless / Remote / VPS)",
            "Manually enter API Key",
        ],
        "xai.ts:552, :594, :620"
    );
}

/// The refusal names what the provider does have, so gaining a login has to
/// change the sentence as well as the table.
#[test]
fn grok_accepts_a_browser_login_and_says_so_when_asked_for_one_it_lacks() {
    assert_eq!(
        accepted(ProviderId::Grok, Method::Browser).expect("grok has a browser login"),
        Method::Browser
    );

    let refused = accepted(ProviderId::Anthropic, Method::Browser)
        .expect_err("anthropic has no OAuth flow in the pin")
        .to_string();
    assert!(refused.contains("`browser`") && refused.contains("`api`"), "{refused}");
}

/// The variable decides where a device code and then a pair of tokens are
/// sent, so anything that could name a host off this machine has to be
/// refused by the shape rather than by a check somebody remembered.
#[test]
fn only_a_whole_loopback_origin_may_redirect_a_login() {
    for origin in ["http://127.0.0.1:8080", "http://localhost:1", "http://[::1]:65535"] {
        assert_eq!(loopback_origin(origin), Some(origin), "{origin}");
    }

    for refused in [
        // The case a prefix match alone would accept: everything before
        // the `@` is userinfo, and this resolves to `elsewhere.example`.
        "http://127.0.0.1:80@elsewhere.example",
        "http://127.0.0.1:8080/oauth2/token",
        "http://127.0.0.1:8080?x=1",
        "http://127.0.0.1:8080#x",
        // Loopback is the whole point; https to anywhere is not the same
        // promise, because a token would then leave the machine.
        "https://auth.x.ai",
        "http://auth.x.ai:80",
        // A suffix match is the shape of bypass this refuses.
        "http://notlocalhost:80",
        "http://localhost.evil.example:80",
        // No port is no origin this could listen on.
        "http://127.0.0.1",
        "http://127.0.0.1:",
        "",
    ] {
        assert_eq!(loopback_origin(refused), None, "{refused}");
    }
}

/// Every deployment a flag can name is resolved without a question being
/// asked, which is the whole of what makes a Copilot login runnable by a
/// machine.
///
/// Nothing here may reach [`super::prompted`]: a test that did would block
/// on standard input under the runner, so the combinations below are
/// exactly the ones an invocation answers for itself.
#[test]
fn a_named_deployment_answers_both_questions_without_asking_either() {
    for (kind, enterprise_url, expected) in [
        (Some(DeploymentKind::Public), None, Deployment::Public),
        (None, Some("https://company.ghe.com/"), Deployment::enterprise("company.ghe.com")),
        (
            Some(DeploymentKind::Enterprise),
            Some("company.ghe.com"),
            Deployment::enterprise("company.ghe.com"),
        ),
    ] {
        let answer = DeploymentAnswer { kind, enterprise_url: enterprise_url.map(str::to_owned) };
        assert_eq!(
            deployment(answer).expect("a named deployment needs nothing else"),
            expected,
            "{kind:?} + {enterprise_url:?}"
        );
    }
}

/// Two flags naming different deployments is a question this build cannot
/// answer, so it refuses instead of picking the one somebody's other flag
/// said not to.
#[test]
fn naming_the_public_deployment_and_an_enterprise_address_is_refused() {
    let refused = deployment(DeploymentAnswer {
        kind: Some(DeploymentKind::Public),
        enterprise_url: Some("company.ghe.com".to_owned()),
    })
    .expect_err("the two flags contradict each other")
    .to_string();

    assert!(
        refused.contains("--deployment public")
            && refused.contains("company.ghe.com")
            && refused.contains("nothing was stored"),
        "{refused}"
    );
}

/// A blank address is the one enterprise spelling that names nothing, and
/// it must not become `https:///login/device/code`.
#[test]
fn an_enterprise_deployment_with_a_blank_address_is_refused() {
    let refused = deployment(DeploymentAnswer {
        kind: Some(DeploymentKind::Enterprise),
        enterprise_url: Some("   ".to_owned()),
    })
    .expect_err("a blank domain is not a deployment")
    .to_string();

    assert!(refused.contains("nothing was stored"), "{refused}");
}

/// The words `--method` takes are the words its refusal names, which is
/// only true while one function writes both.
#[test]
fn a_method_is_spelled_the_same_wherever_it_is_written() {
    assert_eq!(Method::Api.to_string(), "api");
    assert_eq!(Method::Browser.to_string(), "browser");
    assert_eq!(Method::Device.to_string(), "device");

    for method in [Method::Api, Method::Browser, Method::Device] {
        let spelled = method.to_string();
        assert_eq!(
            <Method as clap::ValueEnum>::from_str(&spelled, false),
            Ok(method),
            "`--method {spelled}` has to parse as the method it prints as"
        );
    }
}
