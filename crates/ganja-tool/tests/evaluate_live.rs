//! One real call to TypeSafe, opt in, never in CI.
//!
//! Two locks, the shape the other live suites in this workspace use: the
//! test is `#[ignore]`, so a plain `cargo test` never runs it, **and** it
//! checks `GANJA_LIVE_TEST`, so even `--ignored` does nothing without the
//! variable. Either lock alone would eventually be defeated — `--ignored` is
//! a flag somebody adds to see everything run, and an environment variable
//! is a thing a shell profile exports — and this one costs a third party a
//! request and the owner money.
//!
//! What it is for: the wire this build sends is one the vendor still
//! accepts. It asserts the *shape* of the answer, not its content, because
//! the content is a probability and a model is free to change its mind about
//! anything.
//!
//! ```sh
//! GANJA_LIVE_TEST=1 cargo test -p ganja-tool --test evaluate_live -- --ignored --nocapture
//! ```
//!
//! It needs `TYPESAFE_API_KEY` in the environment and reaches the real
//! `https://api.typesafe.ai`.

use ganja_tool::typesafe::{Answer, Client, Question, Request, Settings, State};
use tokio_util::sync::CancellationToken;

/// The second lock.
const LIVE: &str = "GANJA_LIVE_TEST";

#[tokio::test]
#[ignore = "reaches the real TypeSafe API and costs the owner money"]
async fn one_real_noul_comes_back_as_a_probability_this_build_can_read() {
    if std::env::var(LIVE).is_err() {
        eprintln!("skipped: set {LIVE}=1 to run this");

        return;
    }

    let settings = Settings::from_env()
        .expect("the configured base URL is acceptable")
        .expect("this test needs TYPESAFE_API_KEY");
    let client = Client::new(settings).expect("an HTTP client builds");
    // Nothing from the project: a literal the vendor's own documentation
    // uses, so a live run sends no content belonging to whoever runs it.
    let request = Request::checked(
        State::Text("Help! My payouts have been failing for 3 days.".to_owned()),
        [(
            "urgent".to_owned(),
            Question::Noul {
                instructions: serde_json::json!("Does this convey urgency?"),
                criteria: None,
            },
        )]
        .into_iter()
        .collect(),
        "jev-latest".to_owned(),
    )
    .expect("the request is within every limit");

    let answered =
        client.evaluate(&request, &CancellationToken::new()).await.expect("TypeSafe answered");

    println!("served model: {}", answered.model);
    println!("usage: {:?}", answered.usage);
    println!("answer: {:?}", answered.answers.get("urgent"));

    let Some(Answer::Noul { noul }) = answered.answers.get("urgent") else {
        panic!("a noul question is answered by a noul: {:?}", answered.answers);
    };

    assert!((0.0..=1.0).contains(noul), "a probability, and this one is {noul}");
    assert!(
        answered.usage.input_tokens > 0,
        "the state and the question cost something: {:?}",
        answered.usage
    );
    assert!(!answered.model.is_empty(), "the answer names what judged it");
}
