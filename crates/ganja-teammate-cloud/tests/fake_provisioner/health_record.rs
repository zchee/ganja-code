use super::*;

fn summary(name: &str) -> String {
    let logged = Hosted::delivery_log().logged();
    let summaries = logged
        .lines()
        .filter(|line| line.contains("launch health wait ended") && line.contains(name))
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 1, "exactly one launch summary: {logged}");
    let line = summaries[0];
    let failures = [
        "connection_refused",
        "connection_reset",
        "timed_out",
        "http_3xx",
        "http_4xx",
        "http_5xx",
        "http_other",
        "decode",
        "other",
    ]
    .into_iter()
    .map(|kind| field(line, kind))
    .sum::<u64>();
    assert_eq!(field(line, "failures"), failures, "{line}");
    assert_eq!(
        field(line, "http_status"),
        ["http_3xx", "http_4xx", "http_5xx", "http_other"]
            .into_iter()
            .map(|kind| field(line, kind))
            .sum::<u64>(),
        "{line}"
    );
    assert_eq!(
        field(line, "attempts"),
        failures + field(line, "unfinished") + u64::from(line.contains("outcome=\"ready\"")),
        "{line}"
    );
    line.to_owned()
}

fn field(line: &str, name: &str) -> u64 {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("missing {name}: {line}"))
        .parse()
        .unwrap_or_else(|error| panic!("numeric {name}: {error}; {line}"))
}

async fn wait_pending_health(hosted: &Hosted) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while hosted
            .init_health_wire
            .as_ref()
            .expect("health wire")
            .health_requests
            .load(std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real health request reaches the pending peer");
}

async fn assert_cancelled_start(exit: &str) {
    let hosted = Arc::new(
        Hosted::configured(std::time::Duration::from_secs(5), Some(LaunchHealthFault::Deadline))
            .await,
    );
    let task = tokio::spawn({
        let hosted = Arc::clone(&hosted);
        let exit = exit.to_owned();
        async move {
            let caller = ganja_testkit::caller(hosted.home.path());
            let mut start = Box::pin(hosted.door.start(
                ganja_testkit::spawn_with_prompt(&hosted.name, Some("cloud"), "first request"),
                &caller,
                &ganja_testkit::AllowSpawn,
            ));
            if matches!(exit.as_str(), "abort" | "member") {
                let result = start.await;
                assert!(result.is_err(), "member cancellation refuses launch");
            } else {
                tokio::select! {
                    result = &mut start => panic!("pending start unexpectedly returned: {result:?}"),
                    () = wait_pending_health(&hosted) => {}
                }
                if exit == "unwind" {
                    panic!("unwind a caller with a pending start");
                }
                drop(start);
            }
        }
    });
    wait_pending_health(&hosted).await;
    if exit == "abort" {
        task.abort();
    }
    if exit == "member" {
        hosted.registry.shutdown().await;
    }
    let joined = task.await;
    match exit {
        "abort" => assert!(joined.expect_err("caller aborted").is_cancelled()),
        "unwind" => assert!(joined.expect_err("caller unwound").is_panic()),
        _ => joined.expect("caller dropped its start"),
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !Hosted::delivery_log()
            .logged()
            .lines()
            .any(|line| line.contains("launch health wait ended") && line.contains(&hosted.name))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("engine observes caller cancellation and drops launch");
    let line = summary(&hosted.name);
    Arc::try_unwrap(hosted).ok().expect("caller released fixture").close().await;
    assert!(line.contains("outcome=\"cancelled\""), "{line}");
    assert_eq!(field(&line, "attempts"), 1, "{line}");
    assert_eq!(field(&line, "failures"), 0, "{line}");
    assert_eq!(field(&line, "timed_out"), 0, "{line}");
    assert_eq!(field(&line, "unfinished"), 1, "{line}");
}

#[tokio::test]
async fn dropping_a_real_pending_start_reports_cancellation_not_a_deadline() {
    assert_cancelled_start("drop").await;
}

#[tokio::test]
async fn aborting_a_real_pending_start_reports_cancellation_not_a_deadline() {
    assert_cancelled_start("abort").await;
}

#[tokio::test]
async fn unwinding_a_real_pending_start_reports_cancellation_not_a_deadline() {
    assert_cancelled_start("unwind").await;
}

#[tokio::test]
async fn registry_shutdown_during_a_real_pending_start_reports_cancellation() {
    assert_cancelled_start("member").await;
}

#[tokio::test]
async fn a_clean_launch_reports_zero_health_failures() {
    let hosted = Hosted::new().await;
    hosted.start().await;
    let line = summary(&hosted.name);
    hosted.close().await;
    assert_eq!(field(&line, "attempts"), 1, "{line}");
    assert_eq!(field(&line, "failures"), 0, "{line}");
    for kind in
        ["connection_refused", "connection_reset", "timed_out", "http_status", "decode", "other"]
    {
        assert_eq!(field(&line, kind), 0, "{line}");
    }
    assert!(line.contains("outcome=\"ready\""), "{line}");
    field(&line, "elapsed_ms");
}

#[tokio::test]
async fn a_launch_reports_non_success_health_statuses() {
    let hosted =
        Hosted::configured(std::time::Duration::from_secs(5), Some(LaunchHealthFault::InitRefusal))
            .await;
    hosted.start().await;
    let line = summary(&hosted.name);
    hosted.close().await;
    assert_eq!(field(&line, "attempts"), 3, "{line}");
    assert_eq!(field(&line, "failures"), 2, "{line}");
    assert_eq!(field(&line, "http_status"), 2, "{line}");
    assert_eq!(field(&line, "http_4xx"), 1, "{line}");
    assert_eq!(field(&line, "http_5xx"), 1, "{line}");
    assert!(line.contains("outcome=\"ready\""), "{line}");
}

#[tokio::test]
async fn a_launch_reports_refused_health_attempts_until_the_deadline() {
    let hosted =
        Hosted::configured(std::time::Duration::from_secs(5), Some(LaunchHealthFault::Refused))
            .await;
    let result = hosted
        .door
        .start(
            ganja_testkit::spawn_with_prompt(&hosted.name, Some("cloud"), "first request"),
            &ganja_testkit::caller(hosted.home.path()),
            &ganja_testkit::AllowSpawn,
        )
        .await;
    let line = summary(&hosted.name);
    hosted.close().await;
    assert!(result.is_err(), "closed listener prevents health readiness");
    let refused = field(&line, "connection_refused");
    assert!(refused > 0, "the closed listener refuses real probes: {line}");
    assert_eq!(field(&line, "attempts"), field(&line, "failures"), "{line}");
    assert_eq!(field(&line, "failures"), refused + field(&line, "timed_out"), "{line}");
    assert_eq!(field(&line, "unfinished"), 0, "{line}");
    assert_eq!(field(&line, "other"), 0, "{line}");
    assert!(line.contains("outcome=\"deadline_reached\""), "{line}");
}

#[tokio::test]
async fn a_launch_reports_a_health_connection_reset_by_the_peer() {
    let hosted =
        Hosted::configured(std::time::Duration::from_secs(5), Some(LaunchHealthFault::Reset)).await;
    hosted.start().await;
    let line = summary(&hosted.name);
    hosted.close().await;
    assert_eq!(field(&line, "attempts"), 2, "{line}");
    assert_eq!(field(&line, "failures"), 1, "{line}");
    assert_eq!(field(&line, "connection_reset"), 1, "{line}");
}

#[tokio::test]
async fn a_launch_reports_its_health_deadline_and_in_flight_attempt() {
    let hosted =
        Hosted::configured(std::time::Duration::from_secs(5), Some(LaunchHealthFault::Deadline))
            .await;
    let result = hosted
        .door
        .start(
            ganja_testkit::spawn_with_prompt(&hosted.name, Some("cloud"), "first request"),
            &ganja_testkit::caller(hosted.home.path()),
            &ganja_testkit::AllowSpawn,
        )
        .await;
    let line = summary(&hosted.name);
    hosted.close().await;
    assert!(result.is_err(), "health deadline refuses launch");
    assert_eq!(field(&line, "attempts"), 1, "{line}");
    assert_eq!(field(&line, "failures"), 1, "{line}");
    assert_eq!(field(&line, "timed_out"), 1, "{line}");
    assert!(line.contains("outcome=\"deadline_reached\""), "{line}");
    assert!(field(&line, "elapsed_ms") >= 3000, "{line}");
}

#[tokio::test]
async fn the_enclosing_launch_deadline_still_records_the_health_wait() {
    let hosted =
        Hosted::configured(std::time::Duration::from_secs(1), Some(LaunchHealthFault::Deadline))
            .await;
    let result = hosted
        .door
        .start(
            ganja_testkit::spawn_with_prompt(&hosted.name, Some("cloud"), "first request"),
            &ganja_testkit::caller(hosted.home.path()),
            &ganja_testkit::AllowSpawn,
        )
        .await;
    let line = summary(&hosted.name);
    hosted.close().await;
    assert!(result.is_err(), "launch deadline refuses a pending health wait");
    assert_eq!(field(&line, "attempts"), 1, "{line}");
    assert_eq!(field(&line, "failures"), 1, "{line}");
    assert_eq!(field(&line, "timed_out"), 1, "{line}");
    assert!(line.contains("outcome=\"deadline_reached\""), "{line}");
}

#[tokio::test]
async fn health_decode_failures_never_copy_wire_values_into_the_summary() {
    const CHILD: &str = "GANJA_HEALTH_RECORD_CANARY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let traces = tempfile::tempdir().expect("private trace directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(traces.path(), std::fs::Permissions::from_mode(0o700))
                .expect("private trace permissions");
        }
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "health_record::health_decode_failures_never_copy_wire_values_into_the_summary",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("GANJA_CLOUD_TRACE", traces.path())
            .output()
            .expect("isolated trace canary");
        assert!(
            output.status.success(),
            "canary child: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let hosted =
        Hosted::configured(std::time::Duration::from_secs(5), Some(LaunchHealthFault::Decode))
            .await;
    let trace_path =
        std::path::PathBuf::from(std::env::var_os("GANJA_CLOUD_TRACE").expect("trace path"))
            .join(format!("{}.jsonl", hosted.name));
    hosted.start().await;
    hosted.wait_finished().await;
    let line = summary(&hosted.name);
    hosted.close().await;
    let trace = std::fs::read_to_string(&trace_path).expect("cloud trace written");
    for frame in trace.lines() {
        serde_json::from_str::<ganja_teammate_cloud::trace::TraceFrame>(frame)
            .expect("summary must not change the strict event-trace format");
    }
    assert!(!trace.is_empty(), "positive control: event trace exists");
    assert_eq!(field(&line, "attempts"), 2, "{line}");
    assert_eq!(field(&line, "failures"), 1, "{line}");
    assert_eq!(field(&line, "decode"), 1, "{line}");
    for secret in
        ["test-password", "body-canary-4f32-json", "header-canary-9c81", "dGVzdC1wYXNzd29yZA=="]
    {
        assert!(!line.contains(secret), "wire canary escaped into health summary");
        assert!(
            !Hosted::delivery_log().logged().contains(secret),
            "wire canary escaped into tracing"
        );
        assert!(!trace.contains(secret), "wire canary escaped into cloud trace");
    }
}
