use super::*;

fn summary(name: &str) -> String {
    let logged = Hosted::delivery_log().logged();
    let summaries = logged
        .lines()
        .filter(|line| line.contains("launch health wait ended") && line.contains(name))
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 1, "exactly one launch summary: {logged}");
    summaries[0].to_owned()
}

fn field(line: &str, name: &str) -> u64 {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("missing {name}: {line}"))
        .parse()
        .unwrap_or_else(|error| panic!("numeric {name}: {error}; {line}"))
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
async fn a_launch_reports_refused_health_attempts_before_the_listener_recovers() {
    let hosted =
        Hosted::configured(std::time::Duration::from_secs(5), Some(LaunchHealthFault::Refused))
            .await;
    hosted.start().await;
    let line = summary(&hosted.name);
    hosted.close().await;
    let refused = field(&line, "connection_refused");
    assert!(refused >= 2, "the injected listener outage refuses repeated probes: {line}");
    assert_eq!(field(&line, "attempts"), refused + 1, "{line}");
    assert_eq!(field(&line, "failures"), refused, "{line}");
    assert_eq!(field(&line, "other"), 0, "{line}");
    assert!(line.contains("outcome=\"ready\""), "{line}");
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
