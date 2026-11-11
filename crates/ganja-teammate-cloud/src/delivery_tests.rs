use super::*;

#[test]
fn partial_native_thread_startup_refuses_further_admission() {
    for failure in 0..=WORKERS {
        let lane = Lane::on(Service::start_with_failure(Some(failure)));
        assert_eq!(
            lane.submit(Payload::Read { path: PathBuf::from("unused") }).unwrap_err(),
            DropReason::Unavailable
        );
        assert_eq!(lane.outstanding(), 0);
    }
}

#[test]
fn failed_startup_does_not_retain_departed_lanes() {
    for failure in 0..=WORKERS {
        let service = Service::start_with_failure(Some(failure));
        for _ in 0..128 {
            drop(Lane::on(Arc::clone(&service)));
        }
        assert!(
            service.state.lock().expect("service state").lanes.is_empty(),
            "failed startup at thread {failure} retained closed empty lanes"
        );
    }
}

#[tokio::test]
async fn a_detached_queued_notification_records_its_late_terminal_outcome() {
    for failed in [false, true] {
        let service = Service::start();
        let home = tempfile::tempdir().expect("home");
        let lanes: Vec<_> = (0..=WORKERS).map(|_| Lane::on(Arc::clone(&service))).collect();
        let mut holders = Vec::new();
        let mut heads = Vec::new();
        for (index, lane) in lanes.iter().take(WORKERS).enumerate() {
            let path = home.path().join(format!("held-{index}"));
            mailbox::seed(&path).expect("seed held inbox");
            holders.push(hold(path.clone()));
            let head = lane
                .submit(Payload::Write { path, envelope: Box::new(message("head")) })
                .expect("head admitted");
            started(&head).await;
            heads.push(head);
        }
        let path = home.path().join("queued");
        if failed {
            std::fs::create_dir(&path).expect("real write failure");
        }
        let mut notice = lanes[WORKERS]
            .submit(Payload::Write { path: path.clone(), envelope: Box::new(message("notice")) })
            .expect("queued notice");
        let watch = JobWatch(Arc::clone(&notice.cell));
        assert!(matches!(notice.observe(FINAL_OBSERVATION).await, Observation::PendingQueued));
        notice.detach();
        let (release, holder) = holders.pop().expect("one occupied worker");
        release.send(()).expect("release one worker before expiry");
        holder.join().expect("holder");
        tokio::time::timeout(EXPIRY + FINAL_OBSERVATION + Duration::from_secs(2), watch.settled())
            .await
            .expect("queued notice has a terminal result");
        let trace = watch.snapshot();
        for (release, holder) in holders {
            release.send(()).expect("release remaining worker");
            holder.join().expect("holder");
        }
        assert_eq!(trace.observation, Some("PendingQueued"));
        assert_eq!(trace.outcome, Some(if failed { "LateFailed" } else { "LateSucceeded" }));
        assert!(trace.admitted < trace.started.expect("notice started"));
        assert!(trace.started < trace.completed);
        if !failed {
            assert_eq!(mailbox::read(&path).expect("written notice").valid[0].text, "notice");
        }
        drop(heads);
    }
}

#[tokio::test]
async fn unconsumed_results_hold_the_per_member_limit() {
    let lane = Lane::on(Service::start());
    let home = tempfile::tempdir().expect("home");
    let mut receipts = Vec::new();
    for _ in 0..LANE_CAPACITY {
        receipts.push(
            lane.submit(Payload::Read { path: home.path().join("missing") }).expect("admitted"),
        );
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while !receipts
            .iter()
            .all(|receipt| matches!(*receipt.cell.status.lock().expect("status"), Status::Final(_)))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all reads finish");
    assert_eq!(lane.outstanding(), LANE_CAPACITY);
    assert_eq!(
        lane.submit(Payload::Read { path: home.path().join("missing") }).unwrap_err(),
        DropReason::Full
    );
    receipts.pop();
    lane.submit(Payload::Read { path: home.path().join("missing") })
        .expect("dropped receipt releases credit");
}

fn message(text: &str) -> MailboxMessage {
    MailboxMessage::new("sender", text, ganja_team::record::now_iso8601())
}

#[tokio::test]
async fn a_blocked_final_wait_has_one_total_budget() {
    use futures::FutureExt as _;

    let lane = Lane::on(Service::start());
    let home = tempfile::tempdir().expect("home");
    let path = home.path().join("inbox");
    mailbox::seed(&path).expect("inbox");
    let (release, holder) = hold(path.clone());
    let mut receipt = lane
        .submit(Payload::Write { path, envelope: Box::new(message("blocked")) })
        .expect("write admitted");
    started(&receipt).await;
    let budget = EXPIRY + FINAL_OBSERVATION + TEST_SCHEDULER_ALLOWANCE;
    let result = tokio::time::timeout(
        budget + TEST_SCHEDULER_ALLOWANCE,
        std::panic::AssertUnwindSafe(final_value(&mut receipt)).catch_unwind(),
    )
    .await;
    release.send(()).expect("release blocked write");
    holder.join().expect("holder");
    assert!(
        matches!(result, Ok(Err(_))),
        "the helper must fail at its total deadline, not rely on an outer test timeout"
    );
}

async fn final_value(receipt: &mut Receipt) -> Value {
    tokio::time::timeout(EXPIRY + FINAL_OBSERVATION + TEST_SCHEDULER_ALLOWANCE, async {
        loop {
            if let Observation::Final(value) = receipt.observe(OBSERVATION).await {
                return value;
            }
        }
    })
    .await
    .expect("delivery final result exceeded its total budget")
}

async fn started(receipt: &Receipt) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(*receipt.cell.status.lock().expect("status"), Status::Running) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native operation starts");
}

fn hold(path: PathBuf) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (ready, ready_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let holder = thread::spawn(move || {
        let _held = ganja_team::lock::acquire(&path).expect("real inbox lock");
        ready.send(()).expect("ready");
        release_rx.recv().expect("release");
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).expect("holder ready");
    (release, holder)
}

#[tokio::test]
async fn notification_delivery_preserves_member_order_and_has_a_bound() {
    let service = Service::start();
    let home = tempfile::tempdir().expect("home");
    let lanes: Vec<_> = (0..8).map(|_| Lane::on(Arc::clone(&service))).collect();
    let mut held = Vec::new();
    let mut heads = Vec::new();
    for (index, lane) in lanes.iter().take(WORKERS).enumerate() {
        let path = home.path().join(format!("{index}.json"));
        mailbox::seed(&path).expect("seed");
        held.push(hold(path.clone()));
        let receipt = lane
            .submit(Payload::Write { path, envelope: Box::new(message("head")) })
            .expect("head admitted");
        started(&receipt).await;
        heads.push(receipt);
    }
    let expired_prune_path = home.path().join("expired-prune.json");
    let retained = message("a queued prune must leave this intact");
    mailbox::write(&expired_prune_path, retained.clone()).expect("seed prune candidate");
    let mut queued = Vec::new();
    for (index, lane) in lanes.iter().enumerate() {
        for number in heads.get(index).map_or(0, |_| 1)..LANE_CAPACITY {
            let payload = if index == WORKERS && number == 0 {
                Payload::prune(expired_prune_path.clone(), std::slice::from_ref(&retained))
            } else {
                Payload::Write {
                    path: home.path().join(format!("{index}.json")),
                    envelope: Box::new(message(&number.to_string())),
                }
            };
            queued.push(lane.submit(payload).expect("credit"));
        }
    }
    let overflow = Lane::on(Arc::clone(&service));
    assert_eq!(service.state.lock().expect("state").credits.len(), CAPACITY);
    assert_eq!(
        overflow.submit(Payload::Read { path: home.path().join("other") }).unwrap_err(),
        DropReason::Full,
        "the global cap includes jobs in every lane"
    );
    assert_eq!(
        lanes[0].submit(Payload::Read { path: home.path().join("other") }).unwrap_err(),
        DropReason::Full
    );
    assert_eq!(
        lanes[0]
            .submit(Payload::Write {
                path: home.path().join("other"),
                envelope: Box::new(message(&"x".repeat(PAYLOAD_CAPACITY)))
            })
            .unwrap_err(),
        DropReason::Oversize
    );
    assert!(matches!(heads[0].observe(Duration::ZERO).await, Observation::Late));
    assert!(matches!(queued[0].observe(Duration::ZERO).await, Observation::PendingQueued));
    tokio::time::sleep(EXPIRY + Duration::from_millis(100)).await;
    assert!(queued.iter().all(|receipt| matches!(
        *receipt.cell.status.lock().expect("status"),
        Status::Final(Some(Value::Dropped(DropReason::Expired)))
    )));
    assert_eq!(
        overflow.submit(Payload::Read { path: home.path().join("other") }).unwrap_err(),
        DropReason::Full,
        "unconsumed expired receipts retain credit"
    );
    for receipt in &mut queued {
        assert!(matches!(final_value(receipt).await, Value::Dropped(DropReason::Expired)));
    }
    let mut available = lanes[7]
        .submit(Payload::Read { path: home.path().join("missing") })
        .expect("consumed receipts release credit");
    assert!(matches!(available.observe(Duration::ZERO).await, Observation::PendingQueued));
    for (release, holder) in held {
        release.send(()).expect("release");
        holder.join().expect("holder");
    }
    for receipt in &mut heads {
        assert!(matches!(final_value(receipt).await, Value::Write(Ok(_))));
    }
    assert!(matches!(final_value(&mut available).await, Value::Read(_)));
    let remaining = mailbox::read(&expired_prune_path).expect("expired prune inbox").valid;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].text, retained.text);
    for index in 0..WORKERS {
        assert_eq!(
            mailbox::read(&home.path().join(format!("{index}.json"))).expect("read").valid.len(),
            1,
            "expired followers cannot overtake the late head"
        );
    }
    drop((lanes, overflow));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !service.state.lock().expect("state").lanes.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("closed empty lanes are removed");
    for _ in 0..128 {
        let lane = Lane::on(Arc::clone(&service));
        lane.seal();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while !service.state.lock().expect("state").lanes.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("repeated retirements leave no lane history");
}

#[tokio::test]
async fn a_late_lane_head_cannot_be_overtaken_while_other_workers_are_available() {
    let service = Service::start();
    let lane = Lane::on(Arc::clone(&service));
    let independent = Lane::on(service);
    let home = tempfile::tempdir().expect("home");
    let blocked = home.path().join("blocked");
    mailbox::seed(&blocked).expect("blocked inbox");
    let (release, holder) = hold(blocked.clone());
    let mut head = lane
        .submit(Payload::Write { path: blocked, envelope: Box::new(message("first")) })
        .expect("head");
    started(&head).await;
    assert!(matches!(head.observe(Duration::ZERO).await, Observation::Late));
    let follower_path = home.path().join("follower");
    let mut follower = lane
        .submit(Payload::Write {
            path: follower_path.clone(),
            envelope: Box::new(message("second")),
        })
        .expect("follower");
    let mut other = independent
        .submit(Payload::Read { path: home.path().join("missing") })
        .expect("independent read");
    assert!(
        matches!(final_value(&mut other).await, Value::Read(Ok(_))),
        "another worker can execute"
    );
    assert!(
        matches!(follower.observe(Duration::ZERO).await, Observation::PendingQueued),
        "the late head still serializes this lane"
    );
    assert!(matches!(final_value(&mut follower).await, Value::Dropped(DropReason::Expired)));
    assert!(!follower_path.exists(), "an expired follower never started");
    release.send(()).expect("release head");
    holder.join().expect("holder");
    assert!(matches!(final_value(&mut head).await, Value::Write(Ok(_))));
    let mut third = lane
        .submit(Payload::Write {
            path: follower_path.clone(),
            envelope: Box::new(message("third")),
        })
        .expect("next eligible job");
    assert!(
        third.cell.sequence > follower.cell.sequence && follower.cell.sequence > head.cell.sequence
    );
    assert!(matches!(final_value(&mut third).await, Value::Write(Ok(_))));
    assert_eq!(mailbox::read(&follower_path).expect("follower inbox").valid[0].text, "third");
}

#[tokio::test]
async fn consumed_results_and_dropped_receipts_release_their_admission_credit() {
    let service = Service::start();
    let lane = Lane::on(Arc::clone(&service));
    let home = tempfile::tempdir().expect("home");
    let mut consumed = Vec::new();
    for _ in 0..32 {
        let mut receipt =
            lane.submit(Payload::Read { path: home.path().join("missing") }).expect("admitted");
        assert!(matches!(final_value(&mut receipt).await, Value::Read(_)));
        consumed.push(receipt);
    }
    assert_eq!(lane.outstanding(), 0);
    lane.seal();
    assert_eq!(
        lane.submit(Payload::Read { path: home.path().join("missing") }).unwrap_err(),
        DropReason::Closed
    );
}

#[tokio::test]
async fn a_cancelled_queued_prune_never_starts_after_its_predecessor() {
    let service = Service::start();
    let lane = Lane::on(service);
    let home = tempfile::tempdir().expect("home");
    let path = home.path().join("inbox");
    let envelope = message("keep me");
    mailbox::write(&path, envelope.clone()).expect("seed message");
    let (release, holder) = hold(path.clone());
    let mut head = lane
        .submit(Payload::Write { path: path.clone(), envelope: Box::new(message("head")) })
        .expect("head");
    started(&head).await;
    let prune = lane.submit(Payload::prune(path.clone(), &[envelope])).expect("prune");
    let cell = Arc::clone(&prune.cell);
    drop(prune);
    release.send(()).expect("release");
    holder.join().expect("holder");
    assert!(matches!(final_value(&mut head).await, Value::Write(Ok(_))));
    assert!(matches!(
        *cell.status.lock().expect("status"),
        Status::Final(Some(Value::Dropped(DropReason::Cancelled)))
    ));
    assert_eq!(mailbox::read(&path).expect("read").valid.len(), 2);
}
