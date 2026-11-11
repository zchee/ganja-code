use ganja_client::EventId;
use ganja_client::sse::{Connected, Replay};
use ganja_team::{MemberName, TeamName, TeamsRoot, mailbox};
use tokio::sync::Notify;

use super::*;
use crate::gate::Permit;
use crate::provisioner::{Author, Endpoint, Provisioner, WorkerSpec};
use crate::state::Deadlines;

#[path = "member_recovery_tests.rs"]
mod recovery;

fn hold_inbox(
    path: std::path::PathBuf,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (ready, ready_rx) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _guard = ganja_team::lock::acquire(&path).expect("hold real inbox");
        ready.send(()).expect("holder ready");
        release_rx.recv().expect("release holder");
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).expect("real lock acquired");
    (release, thread)
}

fn cleanup_budget(member: &CloudMember) -> Duration {
    member.runtime.deadlines.cleanup
        + delivery::FINAL_OBSERVATION
        + delivery::TEST_SCHEDULER_ALLOWANCE
}

async fn wait_local_call(member: &CloudMember, kind: &str) {
    timeout(Duration::from_secs(2), async {
        while !match kind {
            "read" | "prune" => member.inbound.running(kind),
            "write" => member.outbound.running(kind),
            _ => panic!("unknown mailbox operation"),
        } {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native mailbox call starts");
}

fn shutdown_mail() -> MailboxMessage {
    MailboxMessage::from_frame(
        "team-lead",
        &Frame::ShutdownRequest(ganja_protocol::team::ShutdownRequest {
            request_id: "held-inbox-shutdown".to_owned(),
            from: "team-lead".to_owned(),
            reason: Some("test shutdown".to_owned()),
            timestamp: record::now_iso8601(),
        }),
        record::now_iso8601(),
    )
    .expect("shutdown envelope")
}

#[tokio::test]
async fn final_observation_closes_both_lanes_while_the_member_is_retained() {
    let fixture = Fixture::new().await;
    fixture.member.cleanup().await;
    // A repeated observation must still seal both lanes before returning early.
    fixture.member.observe_final().await;
    assert_eq!(
        fixture
            .member
            .inbound
            .submit(Payload::Read { path: fixture.member.spec.inbox() })
            .unwrap_err(),
        delivery::DropReason::Closed
    );
    assert_eq!(
        fixture
            .member
            .outbound
            .submit(Payload::Write {
                path: fixture.member.lent.lead_inbox.clone(),
                envelope: Box::new(shutdown_mail()),
            })
            .unwrap_err(),
        delivery::DropReason::Closed
    );
    timeout(delivery::TEST_SCHEDULER_ALLOWANCE, async {
        while fixture.member.inbound.global_counts().0 != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both empty inactive lane records disappear before member Drop");
}

#[tokio::test]
async fn mixed_final_receipts_share_one_budget_in_both_insertion_orders() {
    for inbound_first in [true, false] {
        let fixture = Fixture::new().await;
        let inbox = fixture.member.spec.inbox();
        mailbox::seed(&inbox).expect("member inbox");
        mailbox::seed(&fixture.member.lent.lead_inbox).expect("lead inbox");
        let inbound_hold = hold_inbox(inbox.clone());
        let outbound_hold = hold_inbox(fixture.member.lent.lead_inbox.clone());
        let mut head = fixture
            .member
            .inbound
            .submit(Payload::prune(inbox.clone(), &[]))
            .expect("inbound head");
        wait_local_call(&fixture.member, "prune").await;
        assert!(matches!(head.observe(Duration::ZERO).await, Observation::Late));
        let head_sequence = fixture.member.inbound.last_sequence();
        let prune = Payload::prune(inbox, &[]);
        if inbound_first {
            fixture.member.enqueue(prune);
        }
        fixture.member.mail("mixed outbound".to_owned()).await;
        if !inbound_first {
            fixture.member.enqueue(Payload::prune(fixture.member.spec.inbox(), &[]));
        }
        wait_local_call(&fixture.member, "write").await;
        let outbound = fixture.member.outbound.watch("write");
        // The queued shutdown prune is the second inbound operation.
        let queued = fixture.member.inbound.watch_sequence(head_sequence + 1);
        let observed = timeout(
            delivery::FINAL_OBSERVATION + delivery::TEST_SCHEDULER_ALLOWANCE,
            fixture.member.observe_final(),
        )
        .await;
        let inbound_trace = queued.snapshot();
        let outbound_trace = outbound.snapshot();
        let expired =
            timeout(runner::POLL * 2 + delivery::TEST_SCHEDULER_ALLOWANCE, queued.settled()).await;
        for (release, holder) in [inbound_hold, outbound_hold] {
            release.send(()).expect("release mixed holder");
            holder.join().expect("mixed holder");
        }
        assert!(
            observed.is_ok(),
            "mixed final observation exceeded one final budget plus scheduler allowance"
        );
        assert_eq!(
            inbound_trace.observation,
            Some("PendingQueued"),
            "queued inbound receipt was observed"
        );
        assert_eq!(
            outbound_trace.observation,
            Some("Late"),
            "running outbound receipt was observed"
        );
        let last = if inbound_first { outbound_trace } else { inbound_trace };
        assert!(
            last.observation_budget.expect("second receipt observed")
                < delivery::FINAL_OBSERVATION / 2,
            "the second direction must spend the shared remainder, not a fresh final budget"
        );
        assert!(expired.is_ok(), "queued shutdown prune expires without waiting for its held head");
        assert!(queued.snapshot().started.is_none());
        assert_eq!(queued.snapshot().outcome, Some("Dropped"));
        timeout(delivery::TEST_SCHEDULER_ALLOWANCE, outbound.settled())
            .await
            .expect("outbound settles");
        assert_eq!(outbound.snapshot().outcome, Some("LateSucceeded"));
        assert!(matches!(
            head.observe(delivery::TEST_SCHEDULER_ALLOWANCE).await,
            Observation::Final(Value::Prune(Ok(_)))
        ));
        timeout(delivery::TEST_SCHEDULER_ALLOWANCE, async {
            while fixture.member.inbound.global_counts().0 != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both active lane records disappear after completion");
    }
}

#[tokio::test]
async fn an_outbound_admission_between_seals_is_observed_before_final_return() {
    for admitted_before_close in [true, false] {
        let fixture = Fixture::new().await;
        mailbox::seed(&fixture.member.lent.lead_inbox).expect("lead");
        let (release, holder) = hold_inbox(fixture.member.lent.lead_inbox.clone());
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        *fixture.member.seal_barrier.lock().expect("seal barrier") = Some(Arc::clone(&barrier));
        let member = Arc::clone(&fixture.member);
        let mut final_task = tokio::spawn(async move {
            member.observe_final().await;
        });
        timeout(delivery::TEST_SCHEDULER_ALLOWANCE, barrier.wait()).await.expect("inbound sealed");
        assert_eq!(
            fixture
                .member
                .inbound
                .submit(Payload::Read { path: fixture.member.spec.inbox() })
                .unwrap_err(),
            delivery::DropReason::Closed
        );
        let watch = if admitted_before_close {
            fixture.member.mail("racing outbound".to_owned()).await;
            wait_local_call(&fixture.member, "write").await;
            Some(fixture.member.outbound.watch("write"))
        } else {
            None
        };
        timeout(delivery::TEST_SCHEDULER_ALLOWANCE, barrier.wait())
            .await
            .expect("continue outbound seal");
        let returned = timeout(
            delivery::FINAL_OBSERVATION + delivery::TEST_SCHEDULER_ALLOWANCE,
            &mut final_task,
        )
        .await;
        let observation = watch.as_ref().and_then(|watch| watch.snapshot().observation);
        let refused = fixture
            .member
            .outbound
            .submit(Payload::Write {
                path: fixture.member.lent.lead_inbox.clone(),
                envelope: Box::new(shutdown_mail()),
            })
            .unwrap_err();
        let queue_empty = fixture.member.notifications.lock().expect("notifications").is_empty();
        release.send(()).expect("release race holder");
        holder.join().expect("race holder");
        assert!(returned.is_ok(), "final observation returns within one final budget");
        returned.expect("bounded final task").expect("final task");
        assert_eq!(refused, delivery::DropReason::Closed);
        assert!(queue_empty, "no unobserved retained receipt");
        assert_eq!(observation, admitted_before_close.then_some("Late"));
        if let Some(watch) = watch {
            timeout(delivery::TEST_SCHEDULER_ALLOWANCE, watch.settled())
                .await
                .expect("race settles");
            assert_eq!(watch.snapshot().outcome, Some("LateSucceeded"));
        }
        timeout(delivery::TEST_SCHEDULER_ALLOWANCE, async {
            while fixture.member.inbound.global_counts().0 != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("no lane records with member retained");
    }
}

#[tokio::test]
async fn two_members_can_saturate_both_directions_without_blocking_external_retirement() {
    let mut fixtures = Vec::new();
    let mut holders = Vec::new();
    let mut watches = Vec::new();
    for _ in 0..2 {
        let mut fixture = Fixture::new().await;
        let member = Arc::get_mut(&mut fixture.member).expect("unique member");
        Arc::get_mut(&mut member.runtime).expect("unique runtime").deadlines.cleanup =
            Duration::from_millis(100);
        mailbox::seed(&fixture.member.spec.inbox()).expect("member inbox");
        mailbox::seed(&fixture.member.lent.lead_inbox).expect("lead inbox");
        holders.push(hold_inbox(fixture.member.spec.inbox()));
        holders.push(hold_inbox(fixture.member.lent.lead_inbox.clone()));
        fixture.member.enqueue(Payload::prune(fixture.member.spec.inbox(), &[]));
        fixture.member.mail("saturated outbound".to_owned()).await;
        wait_local_call(&fixture.member, "prune").await;
        wait_local_call(&fixture.member, "write").await;
        watches.push(fixture.member.inbound.watch("prune"));
        watches.push(fixture.member.outbound.watch("write"));
        fixtures.push(fixture);
    }
    let mut third = Fixture::new().await;
    let member = Arc::get_mut(&mut third.member).expect("unique member");
    Arc::get_mut(&mut member.runtime).expect("unique runtime").deadlines.cleanup =
        Duration::from_millis(100);
    let mut queued = third
        .member
        .inbound
        .submit(Payload::Read { path: third.member.spec.inbox() })
        .expect("third read queued");
    let queued_watch = third.member.inbound.watch("read");
    let queued_observation = queued.observe(delivery::OBSERVATION).await;
    let mut runner = tokio::spawn(Arc::clone(&third.member).run());
    let mut retirement = CloudControl(Arc::clone(&third.member))
        .admit(ControlAction::Retire)
        .expect("external retire");
    let retired = timeout(cleanup_budget(&third.member), retirement.wait()).await;
    let returned =
        timeout(delivery::OBSERVATION + cleanup_budget(&third.member), &mut runner).await;
    let counts = third.member.inbound.global_counts();
    let starts: Vec<_> = watches.iter().map(|watch| watch.snapshot().started).collect();
    let phase = third.member.phase();
    let killed = third.provisioner.calls.lock().expect("calls").contains(&"kill");
    let expired =
        timeout(runner::POLL * 2 + delivery::TEST_SCHEDULER_ALLOWANCE, queued_watch.settled())
            .await;
    for (release, holder) in holders {
        release.send(()).expect("release saturated holder");
        holder.join().expect("saturated holder");
    }
    assert_eq!(
        retired.expect("external retirement stays bounded at saturation"),
        ControlOutcome::Completed
    );
    returned.expect("third runner bounded despite saturation").expect("third runner");
    assert!(matches!(queued_observation, Observation::PendingQueued));
    assert!(starts.iter().all(Option::is_some), "both directions really execute for both members");
    assert_eq!(counts.2, 4, "exactly four native workers occupied");
    assert_eq!(phase, Phase::Gone);
    assert!(killed);
    assert!(expired.is_ok());
    assert!(queued_watch.snapshot().started.is_none(), "third member never occupied a worker");
    assert!(matches!(
        queued.observe(Duration::ZERO).await,
        Observation::Final(Value::Dropped(delivery::DropReason::Expired))
    ));
    for watch in watches {
        timeout(delivery::TEST_SCHEDULER_ALLOWANCE, watch.settled())
            .await
            .expect("saturated call settles");
    }
    for fixture in fixtures {
        fixture.member.cleanup().await;
    }
}

#[tokio::test]
async fn each_direction_is_ordered_alone_and_neither_orders_the_other() {
    let fixture = Fixture::new().await;
    let inbox = fixture.member.spec.inbox();
    mailbox::seed(&inbox).expect("member inbox");
    mailbox::seed(&fixture.member.lent.lead_inbox).expect("lead inbox");
    let inbound_hold = hold_inbox(inbox.clone());
    let outbound_hold = hold_inbox(fixture.member.lent.lead_inbox.clone());
    let mut inbound_head =
        fixture.member.inbound.submit(Payload::prune(inbox.clone(), &[])).expect("inbound head");
    let mut outbound_head = fixture
        .member
        .outbound
        .submit(Payload::Write {
            path: fixture.member.lent.lead_inbox.clone(),
            envelope: Box::new(shutdown_mail()),
        })
        .expect("outbound head");
    wait_local_call(&fixture.member, "prune").await;
    wait_local_call(&fixture.member, "write").await;
    let inbound_follower = fixture
        .member
        .inbound
        .submit(Payload::Read { path: inbox.clone() })
        .expect("inbound follower");
    let outbound_follower = fixture
        .member
        .outbound
        .submit(Payload::Write {
            path: fixture.member.lent.lead_inbox.clone(),
            envelope: Box::new(shutdown_mail()),
        })
        .expect("outbound follower");
    let watches = [
        fixture.member.inbound.watch("prune"),
        fixture.member.inbound.watch("read"),
        fixture.member.outbound.watch_sequence(1),
        fixture.member.outbound.watch_sequence(2),
    ];
    // Held heads let both queued followers expire first: out-of-order drop
    // completion is permitted, a follower syscall is not.
    for follower in [&watches[1], &watches[3]] {
        timeout(runner::POLL * 2 + delivery::TEST_SCHEDULER_ALLOWANCE, follower.settled())
            .await
            .expect("queued follower expires behind its held head");
    }
    let snapshots = watches.each_ref().map(|watch| watch.snapshot());
    for (release, holder) in [inbound_hold, outbound_hold] {
        release.send(()).expect("release order holder");
        holder.join().expect("order holder");
    }
    for pair in [[&snapshots[0], &snapshots[1]], [&snapshots[2], &snapshots[3]]] {
        assert!(pair[0].admitted < pair[1].admitted, "admission increases within a direction");
        assert!(
            pair[0].started.is_some() && pair[1].started.is_none(),
            "no follower syscall starts until its active predecessor completes"
        );
        assert_eq!(
            pair[1].outcome,
            Some("Dropped"),
            "a queued expiry reports its explicit drop outcome"
        );
        assert!(
            pair[0].completed.is_none() && pair[1].completed.is_some(),
            "a queued expiry may complete before its held running head"
        );
    }
    assert!(matches!(final_observed_value(&mut inbound_head).await, Value::Prune(Ok(_))));
    assert!(matches!(final_observed_value(&mut outbound_head).await, Value::Write(Ok(_))));
    let finals = watches.each_ref().map(|watch| watch.snapshot());
    assert!(
        finals[0].started.expect("inbound start")
            < finals[0].completed.expect("inbound completion")
    );
    assert!(
        finals[2].started.expect("outbound start")
            < finals[2].completed.expect("outbound completion")
    );
    let mut inbound_third = fixture
        .member
        .inbound
        .submit(Payload::Read { path: inbox.clone() })
        .expect("next inbound read");
    let mut inbound_fourth =
        fixture.member.inbound.submit(Payload::prune(inbox, &[])).expect("next inbound prune");
    let mut outbound_third = fixture
        .member
        .outbound
        .submit(Payload::Write {
            path: fixture.member.lent.lead_inbox.clone(),
            envelope: Box::new(shutdown_mail()),
        })
        .expect("next outbound write");
    let mut outbound_fourth = fixture
        .member
        .outbound
        .submit(Payload::Write {
            path: fixture.member.lent.lead_inbox.clone(),
            envelope: Box::new(shutdown_mail()),
        })
        .expect("following outbound write");
    let successful = [
        fixture.member.inbound.watch_sequence(3),
        fixture.member.inbound.watch_sequence(4),
        fixture.member.outbound.watch_sequence(3),
        fixture.member.outbound.watch_sequence(4),
    ];
    assert!(matches!(final_observed_value(&mut inbound_third).await, Value::Read(Ok(_))));
    assert!(matches!(final_observed_value(&mut inbound_fourth).await, Value::Prune(Ok(_))));
    assert!(matches!(final_observed_value(&mut outbound_third).await, Value::Write(Ok(_))));
    assert!(matches!(final_observed_value(&mut outbound_fourth).await, Value::Write(Ok(_))));
    for (head, pair) in [finals[0], finals[2]].into_iter().zip(successful.as_chunks::<2>().0) {
        let first = pair[0].snapshot();
        let second = pair[1].snapshot();
        assert!(pair[0].sequence() < pair[1].sequence());
        assert!(head.admitted < first.admitted && first.admitted < second.admitted);
        assert!(head.completed.expect("head finished") < first.started.expect("third started"));
        assert!(first.started < first.completed && first.completed < second.started);
        assert!(second.started < second.completed);
    }
    // The directions interleave freely: assert only within-lane order, never between lanes.
    drop((inbound_follower, outbound_follower));
}

async fn final_observed_value(receipt: &mut Receipt) -> Value {
    timeout(delivery::TEST_SCHEDULER_ALLOWANCE, async {
        loop {
            if let Observation::Final(value) = receipt.observe(delivery::OBSERVATION).await {
                return value;
            }
        }
    })
    .await
    .expect("released head reaches its final value")
}

#[tokio::test]
async fn retained_receipts_cap_each_direction_separately_and_count_globally() {
    let mut fixtures = Vec::new();
    let mut retained = Vec::new();
    for _ in 0..4 {
        let fixture = Fixture::new().await;
        let home = fixture._home.path();
        for index in 0..8 {
            retained.push(
                fixture
                    .member
                    .inbound
                    .submit(Payload::Read { path: home.join(format!("missing-{index}")) })
                    .expect("inbound credit"),
            );
            retained.push(
                fixture
                    .member
                    .outbound
                    .submit(Payload::Write {
                        path: home.join(format!("out-{index}.json")),
                        envelope: Box::new(shutdown_mail()),
                    })
                    .expect("outbound credit"),
            );
        }
        timeout(delivery::TEST_SCHEDULER_ALLOWANCE, async {
            for sequence in 1..=8 {
                fixture.member.inbound.watch_sequence(sequence).settled().await;
                fixture.member.outbound.watch_sequence(sequence).settled().await;
            }
        })
        .await
        .expect("retained completed-but-unconsumed receipts settle");
        assert_eq!(fixture.member.inbound.outstanding(), 8);
        assert_eq!(fixture.member.outbound.outstanding(), 8);
        assert_eq!(
            fixture.member.inbound.submit(Payload::Read { path: home.join("ninth") }).unwrap_err(),
            delivery::DropReason::Full,
            "the ninth inbound job is refused while finals stay unconsumed"
        );
        assert_eq!(
            fixture
                .member
                .outbound
                .submit(Payload::Write {
                    path: home.join("ninth.json"),
                    envelope: Box::new(shutdown_mail())
                })
                .unwrap_err(),
            delivery::DropReason::Full,
            "the ninth outbound job is refused while finals stay unconsumed"
        );
        fixtures.push(fixture);
    }
    let extra = Fixture::new().await;
    assert_eq!(
        extra.member.inbound.global_counts().1,
        64,
        "four pairs of lanes share 64 completed-but-unconsumed credits"
    );
    assert_eq!(
        extra.member.inbound.submit(Payload::Read { path: extra.member.spec.inbox() }).unwrap_err(),
        delivery::DropReason::Full
    );
    assert_eq!(
        extra
            .member
            .outbound
            .submit(Payload::Write {
                path: extra.member.lent.lead_inbox.clone(),
                envelope: Box::new(shutdown_mail())
            })
            .unwrap_err(),
        delivery::DropReason::Full
    );
    drop(retained);
    extra
        .member
        .inbound
        .submit(Payload::Read { path: extra.member.spec.inbox() })
        .expect("dropped receipts release their credits");
}

#[tokio::test]
async fn a_blocked_notification_does_not_hold_the_transition_lock() {
    let fixture = Fixture::new().await;
    mailbox::seed(&fixture.member.lent.lead_inbox).expect("seed");
    let (release, holder) = hold_inbox(fixture.member.lent.lead_inbox.clone());
    fixture.provisioner.failures.lock().expect("failures").insert("suspend", "ambiguous");
    timeout(Duration::from_secs(1), fixture.member.quiesce())
        .await
        .expect("quiesce returns while notification is blocked");
    wait_local_call(&fixture.member, "write").await;
    let mut operation = CloudControl(Arc::clone(&fixture.member))
        .admit(ControlAction::ReportExternalRevert)
        .expect("report admitted");
    let outcome = timeout(Duration::from_secs(1), operation.wait()).await;
    let empty =
        mailbox::read(&fixture.member.lent.lead_inbox).expect("read unlocked").valid.is_empty();
    release.send(()).expect("release");
    holder.join().expect("holder");
    assert_eq!(outcome, Ok(ControlOutcome::Completed));
    assert!(empty, "no delivery success while the real lock is held");
    fixture.flush_notifications().await;
}

#[tokio::test]
async fn a_blocked_outbound_write_does_not_delay_noticing_shutdown() {
    let mut fixture = Fixture::new().await;
    let member = Arc::get_mut(&mut fixture.member).expect("unique member");
    Arc::get_mut(&mut member.runtime).expect("unique runtime").deadlines.cleanup =
        Duration::from_millis(100);
    mailbox::seed(&fixture.member.lent.lead_inbox).expect("lead");
    let (release, holder) = hold_inbox(fixture.member.lent.lead_inbox.clone());
    fixture.member.mail("before shutdown".to_owned()).await;
    wait_local_call(&fixture.member, "write").await;
    let first = fixture.member.outbound.watch("write");
    let mut receipt =
        fixture.member.notifications.lock().expect("receipts").pop_front().expect("notice");
    assert!(matches!(receipt.observe(delivery::OBSERVATION).await, Observation::Late));
    fixture.member.notifications.lock().expect("receipts").push_back(receipt);
    mailbox::write(&fixture.member.spec.inbox(), shutdown_mail())
        .expect("shutdown after blocked write");
    let mut runner = tokio::spawn(Arc::clone(&fixture.member).run());
    let bound = runner::POLL + delivery::OBSERVATION + cleanup_budget(&fixture.member);
    let returned = timeout(bound, &mut runner).await;
    let killed = fixture.provisioner.calls.lock().expect("calls").contains(&"kill");
    let phase = fixture.member.phase();
    let worker = fixture.provisioner.current(&fixture.worker);
    let lead_while_held = mailbox::read(&fixture.member.lent.lead_inbox).expect("lead").valid;
    let first_while_held = first.snapshot();
    let ack = returned.is_ok().then(|| fixture.member.outbound.watch_sequence(2));
    release.send(()).expect("release lead");
    holder.join().expect("lead holder");
    if returned.is_err() {
        fixture.member.lent.cancel.cancel();
        timeout(cleanup_budget(&fixture.member), runner)
            .await
            .expect("reap runner")
            .expect("runner");
    }
    assert!(
        returned.is_ok(),
        "Shutdown runner exceeded poll + observation + cleanup + final observation + scheduler allowance"
    );
    returned.expect("bounded Shutdown runner").expect("Shutdown runner completed");
    assert!(killed, "Shutdown must attempt deletion while the lead lock is held");
    assert_eq!(phase, Phase::Gone);
    assert_eq!(worker, WorkerState::Gone);
    assert!(lead_while_held.is_empty(), "no acknowledgement succeeds while held");
    assert!(first_while_held.outcome.is_none());
    timeout(delivery::TEST_SCHEDULER_ALLOWANCE, first.settled())
        .await
        .expect("late notice settles");
    assert_eq!(first.snapshot().outcome, Some("LateSucceeded"));
    let ack = ack.expect("shutdown acknowledgement admitted");
    timeout(delivery::TEST_SCHEDULER_ALLOWANCE, ack.settled()).await.expect("ack settles");
    let messages = mailbox::read(&fixture.member.lent.lead_inbox).expect("lead").valid;
    assert_eq!(messages[0].text, "before shutdown");
    let ack_trace = ack.snapshot();
    match ack_trace.outcome {
        Some("LateSucceeded") => {
            assert!(
                first.snapshot().completed < ack_trace.started,
                "outbound notice precedes acknowledgement"
            );
            assert_eq!(messages.len(), 2);
            assert!(matches!(
                serde_json::from_str::<Frame>(&messages[1].text),
                Ok(Frame::ShutdownApproved(_))
            ));
        }
        Some("Dropped") => {
            assert!(ack_trace.started.is_none(), "a dropped ack never executed");
            assert_eq!(messages.len(), 1);
        }
        outcome => panic!("ack must honestly finish late or drop: {outcome:?}"),
    }
    assert_eq!(
        fixture
            .provisioner
            .calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|call| **call == "kill")
            .count(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_blocked_inbound_read_does_not_delay_an_outbound_notice() {
    let fixture = Fixture::new().await;
    let inbox = fixture.member.spec.inbox();
    std::fs::create_dir_all(inbox.parent().expect("inbox parent")).expect("parent");
    assert!(
        std::process::Command::new("/usr/bin/mkfifo")
            .args(["-m", "600"])
            .arg(&inbox)
            .status()
            .expect("mkfifo")
            .success()
    );
    let (paired, paired_rx) = oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let _writer =
            std::fs::OpenOptions::new().write(true).open(inbox).expect("FIFO writer paired");
        paired.send(()).expect("paired signal");
        release_rx.recv().expect("release FIFO writer");
    });
    let mut runner = tokio::spawn(Arc::clone(&fixture.member).run());
    timeout(delivery::TEST_SCHEDULER_ALLOWANCE, paired_rx)
        .await
        .expect("real read-only open pairs")
        .expect("paired");
    wait_local_call(&fixture.member, "read").await;
    fixture.member.mail("independent outbound notice".to_owned()).await;
    let mut receipt =
        fixture.member.notifications.lock().expect("receipts").pop_front().expect("notice");
    let observed = receipt.observe(delivery::OBSERVATION).await;
    let messages_while_blocked =
        mailbox::read(&fixture.member.lent.lead_inbox).expect("lead").valid;
    let read_still_running = fixture.member.inbound.running("read");
    fixture.member.lent.cancel.cancel();
    let returned = timeout(cleanup_budget(&fixture.member), &mut runner).await;
    release.send(()).expect("release EOF");
    writer.join().expect("writer");
    if returned.is_err() {
        timeout(cleanup_budget(&fixture.member), runner)
            .await
            .expect("reap runner")
            .expect("runner");
    }
    assert!(
        matches!(observed, Observation::Final(Value::Write(Ok(_)))),
        "outbound notice must succeed within observation while the real FIFO Read is blocked: {observed:?}"
    );
    assert!(read_still_running);
    assert_eq!(messages_while_blocked.len(), 1);
    assert_eq!(messages_while_blocked[0].text, "independent outbound notice");
    assert!(returned.is_ok(), "cancelled runner returns before FIFO EOF");
}

#[tokio::test]
async fn a_held_member_inbox_does_not_block_shutdown_cleanup() {
    let fixture = Fixture::new().await;
    mailbox::write(&fixture.member.spec.inbox(), shutdown_mail()).expect("shutdown");
    let (release, holder) = hold_inbox(fixture.member.spec.inbox());
    let runner = tokio::spawn(Arc::clone(&fixture.member).run());
    timeout(Duration::from_secs(2), runner)
        .await
        .expect("runner returns while prune is blocked")
        .expect("runner");
    assert_eq!(fixture.member.phase(), Phase::Gone);
    assert_eq!(fixture.provisioner.current(&fixture.worker), WorkerState::Gone);
    assert!(fixture.provisioner.calls.lock().expect("calls").contains(&"kill"));
    assert_eq!(mailbox::read(&fixture.member.spec.inbox()).expect("read").valid.len(), 1);
    release.send(()).expect("release");
    holder.join().expect("holder");
}

#[tokio::test]
async fn a_held_lead_inbox_does_not_pin_shutdown_acknowledgement() {
    let fixture = Fixture::new().await;
    mailbox::seed(&fixture.member.lent.lead_inbox).expect("lead");
    mailbox::write(&fixture.member.spec.inbox(), shutdown_mail()).expect("shutdown");
    let (release, holder) = hold_inbox(fixture.member.lent.lead_inbox.clone());
    let runner = tokio::spawn(Arc::clone(&fixture.member).run());
    timeout(Duration::from_secs(2), runner)
        .await
        .expect("runner returns with held ack")
        .expect("runner");
    assert_eq!(fixture.member.phase(), Phase::Gone);
    assert!(mailbox::read(&fixture.member.lent.lead_inbox).expect("lead").valid.is_empty());
    wait_local_call(&fixture.member, "write").await;
    release.send(()).expect("release");
    holder.join().expect("holder");
    timeout(Duration::from_secs(2), async {
        loop {
            let messages = mailbox::read(&fixture.member.lent.lead_inbox).expect("lead").valid;
            if !messages.is_empty() {
                assert_eq!(messages.len(), 1);
                assert!(matches!(
                    serde_json::from_str::<Frame>(&messages[0].text),
                    Ok(Frame::ShutdownApproved(_))
                ));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("one late acknowledgement");
}

#[tokio::test]
async fn repeated_retirements_bound_payloads_without_retaining_members() {
    let observer = Lane::new();
    let mut holders = Vec::new();
    let mut homes = Vec::new();
    for index in 0..12 {
        let mut fixture = Fixture::new().await;
        let member = Arc::get_mut(&mut fixture.member).expect("unique member");
        member.spec.name = MemberName::parse(&format!("retiring-{index}")).expect("unique name");
        Arc::get_mut(&mut member.runtime).expect("unique runtime").deadlines.cleanup =
            Duration::from_millis(100);
        mailbox::seed(&fixture.member.lent.lead_inbox).expect("lead inbox");
        if index < 4 {
            holders.push(hold_inbox(fixture.member.lent.lead_inbox.clone()));
        }
        fixture.member.mail("retirement notice".to_owned()).await;
        if index < 4 {
            wait_local_call(&fixture.member, "write").await;
        }
        let weak = Arc::downgrade(&fixture.member);
        if timeout(cleanup_budget(&fixture.member), fixture.member.cleanup()).await.is_err() {
            for (release, holder) in holders {
                release.send(()).expect("release inbox after cleanup timeout");
                holder.join().expect("holder");
            }
            panic!("cleanup did not return within its bound with occupied workers");
        }
        assert_eq!(fixture.member.phase(), Phase::Gone);
        assert_eq!(fixture.provisioner.current(&fixture.worker), WorkerState::Gone);
        let Fixture { _home, member, source, provisioner, worker } = fixture;
        drop((member, source, provisioner, worker));
        assert!(weak.upgrade().is_none(), "retired member {index} was retained by payload work");
        homes.push(_home);
        let (_, outstanding, active) = observer.global_counts();
        assert!(outstanding <= 64 && active <= 4);
    }
    timeout(Duration::from_secs(2), async {
        loop {
            let (lanes, outstanding, active) = observer.global_counts();
            if lanes == 5 && outstanding == 4 && active == 4 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("only four blocked jobs and their lanes remain after queued expiry");
    for (release, holder) in holders {
        release.send(()).expect("release inbox");
        holder.join().expect("holder");
    }
    timeout(Duration::from_secs(2), async {
        while observer.global_counts() != (1, 0, 0) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late completion removes all departed member lanes");
    drop(homes);
}

#[tokio::test]
async fn blocked_payload_work_does_not_retain_the_member() {
    let mut fixture = Fixture::new().await;
    let member = Arc::get_mut(&mut fixture.member).expect("unique member");
    Arc::get_mut(&mut member.runtime).expect("unique runtime").deadlines.cleanup =
        Duration::from_millis(100);
    mailbox::seed(&fixture.member.lent.lead_inbox).expect("lead");
    let (release, holder) = hold_inbox(fixture.member.lent.lead_inbox.clone());
    fixture.member.mail("late notification".to_owned()).await;
    wait_local_call(&fixture.member, "write").await;
    mailbox::seed(&fixture.member.spec.inbox()).expect("member inbox");
    let (inbound_release, inbound_holder) = hold_inbox(fixture.member.spec.inbox());
    fixture.member.enqueue(Payload::prune(fixture.member.spec.inbox(), &[]));
    wait_local_call(&fixture.member, "prune").await;
    let mut runner = tokio::spawn(Arc::clone(&fixture.member).run());
    let weak = Arc::downgrade(&fixture.member);
    if timeout(cleanup_budget(&fixture.member), fixture.member.cleanup()).await.is_err() {
        release.send(()).expect("release inbox after cleanup timeout");
        holder.join().expect("holder");
        inbound_release.send(()).expect("release inbound after cleanup timeout");
        inbound_holder.join().expect("inbound holder");
        timeout(cleanup_budget(&fixture.member), runner)
            .await
            .expect("runner reaped")
            .expect("runner");
        panic!("cleanup did not return within its bound while both directions were blocked");
    }
    let returned =
        timeout(delivery::OBSERVATION + cleanup_budget(&fixture.member), &mut runner).await;
    if returned.is_err() {
        release.send(()).expect("release outbound after runner timeout");
        holder.join().expect("outbound holder");
        inbound_release.send(()).expect("release inbound after runner timeout");
        inbound_holder.join().expect("inbound holder");
        timeout(cleanup_budget(&fixture.member), runner)
            .await
            .expect("runner reaped")
            .expect("runner");
        panic!("runner did not return within its bound while both directions were blocked");
    }
    returned.expect("bounded runner").expect("runner");
    assert_eq!(fixture.member.phase(), Phase::Gone);
    let Fixture { _home, member, source, provisioner, worker } = fixture;
    drop((member, source, provisioner, worker));
    let freed_while_blocked = weak.upgrade().is_none();
    release.send(()).expect("release");
    holder.join().expect("holder");
    inbound_release.send(()).expect("release inbound");
    inbound_holder.join().expect("inbound holder");
    assert!(freed_while_blocked, "native payload or receipt retained the member");
    drop(_home);
}

#[cfg(unix)]
#[tokio::test]
async fn a_fifo_read_is_interrupted_by_retirement_without_waiting_for_eof() {
    let fixture = Fixture::new().await;
    let inbox = fixture.member.spec.inbox();
    std::fs::create_dir_all(inbox.parent().expect("inbox parent")).expect("parent");
    assert!(
        std::process::Command::new("/usr/bin/mkfifo")
            .args(["-m", "600"])
            .arg(&inbox)
            .status()
            .expect("mkfifo")
            .success()
    );
    let (paired, paired_rx) = oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let _writer =
            std::fs::OpenOptions::new().write(true).open(inbox).expect("FIFO writer paired");
        paired.send(()).expect("paired signal");
        release_rx.recv().expect("release FIFO writer");
    });
    let runner = tokio::spawn(Arc::clone(&fixture.member).run());
    timeout(Duration::from_secs(2), paired_rx)
        .await
        .expect("real read-only open pairs")
        .expect("paired");
    let mut retirement =
        CloudControl(Arc::clone(&fixture.member)).admit(ControlAction::Retire).expect("retire");
    assert_eq!(
        timeout(Duration::from_secs(2), retirement.wait()).await.expect("retired"),
        ControlOutcome::Completed
    );
    timeout(Duration::from_secs(2), runner).await.expect("runner without EOF").expect("runner");
    assert_eq!(fixture.provisioner.current(&fixture.worker), WorkerState::Gone);
    release.send(()).expect("release EOF");
    writer.join().expect("writer");
}

// A unit-test target is a different crate instance from the library linked by
// testkit's dev-only cycle. This local double must implement this instance's trait.
#[derive(Debug)]
struct TestProvisioner {
    endpoint: Endpoint,
    state: Mutex<WorkerState>,
    suspended: std::sync::atomic::AtomicUsize,
    calls: Mutex<Vec<&'static str>>,
    holds: Mutex<BTreeMap<&'static str, Arc<Notify>>>,
    failures: Mutex<BTreeMap<&'static str, &'static str>>,
    called: Notify,
    resume_terminal: Mutex<Option<ProvisionErrorKind>>,
}

impl TestProvisioner {
    fn from_endpoint(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            state: Mutex::new(WorkerState::Suspended),
            suspended: std::sync::atomic::AtomicUsize::new(0),
            calls: Mutex::new(Vec::new()),
            holds: Mutex::new(BTreeMap::new()),
            failures: Mutex::new(BTreeMap::new()),
            called: Notify::new(),
            resume_terminal: Mutex::new(None),
        }
    }
    async fn before(
        &self,
        operation: &'static str,
    ) -> Result<(), crate::provisioner::ProvisionError> {
        self.calls.lock().expect("calls").push(operation);
        self.called.notify_one();
        let hold = self.holds.lock().expect("holds").remove(operation);
        if let Some(hold) = hold {
            hold.notified().await;
        }
        if let Some(operation) = self.failures.lock().expect("failures").remove(operation) {
            return Err(crate::provisioner::ProvisionError {
                operation,
                kind: ProvisionErrorKind::Ambiguous,
            });
        }
        Ok(())
    }
    fn current(&self, _worker: &Worker) -> WorkerState {
        self.state.lock().expect("state").clone()
    }
    fn owner_recover(&self, _worker: &Worker) {
        *self.state.lock().expect("state") = WorkerState::Suspended;
    }
}

#[async_trait]
impl Provisioner for TestProvisioner {
    async fn spawn(&self, spec: &WorkerSpec) -> Result<Worker, crate::provisioner::ProvisionError> {
        Ok(Worker {
            handle: crate::provisioner::WorkerHandle::new(spec.name.as_str()),
            endpoint: self.endpoint.clone(),
        })
    }
    async fn resume(
        &self,
        _worker: &Worker,
    ) -> Result<crate::provisioner::Resumed, crate::provisioner::ProvisionError> {
        self.before("resume").await?;
        if let Some(kind) = self.resume_terminal.lock().expect("terminal failure").take() {
            return Err(crate::provisioner::ProvisionError { operation: "resume", kind });
        }
        let mut state = self.state.lock().expect("state");
        let was_suspended = *state == WorkerState::Suspended;
        *state = WorkerState::Running;
        Ok(crate::provisioner::Resumed { was_suspended })
    }
    async fn suspend(
        &self,
        _worker: &Worker,
    ) -> Result<crate::provisioner::SuspendOutcome, crate::provisioner::ProvisionError> {
        self.before("suspend").await?;
        self.suspended.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().expect("state");
        let outcome = if *state == WorkerState::Suspended {
            crate::provisioner::SuspendOutcome::AlreadySuspended
        } else {
            crate::provisioner::SuspendOutcome::Suspended
        };
        *state = WorkerState::Suspended;
        Ok(outcome)
    }
    async fn state(
        &self,
        worker: &Worker,
    ) -> Result<WorkerState, crate::provisioner::ProvisionError> {
        self.before("state").await?;
        Ok(self.current(worker))
    }
    async fn kill(&self, _worker: &Worker) -> Result<(), crate::provisioner::ProvisionError> {
        self.before("kill").await?;
        *self.state.lock().expect("state") = WorkerState::Gone;
        Ok(())
    }
    fn describe(&self, _worker: &Worker) -> String {
        "unit worker".to_owned()
    }
}

#[derive(Debug)]
struct QuietSource {
    gate: Arc<WorkerGate>,
    permit: Mutex<Option<Permit>>,
    held: AtomicBool,
    changed: Notify,
    hold_started: Notify,
    hold_release: Mutex<Option<Arc<Notify>>>,
    invalidations: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl EventSource for QuietSource {
    async fn connect(&self) -> Result<Option<Connected>, ClientError> {
        let Some(mut permit) = self.gate.admit(Door::Stream) else { return Ok(None) };
        permit.classify(AttemptOutcome::Serve);
        *self.permit.lock().expect("stream permit") = Some(permit);
        Ok(Some(Connected { head: cursor(), replay: Replay::Fresh }))
    }

    async fn next(&self) -> Option<Result<Resumed, ClientError>> {
        let notified = self.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.held.load(Ordering::SeqCst) {
            notified.await;
        }
        None
    }

    async fn hold(&self) {
        self.held.store(true, Ordering::SeqCst);
        self.hold_started.notify_one();
        let release = self.hold_release.lock().expect("hold release").take();
        if let Some(release) = release {
            release.notified().await;
        }
        self.permit.lock().expect("stream permit").take();
        self.changed.notify_waiters();
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
    }
    async fn invalidate(&self) {
        self.invalidations.fetch_add(1, Ordering::SeqCst);
        self.hold().await;
    }
    async fn cursor(&self) -> Option<EventId> {
        Some(cursor())
    }
}

fn cursor() -> EventId {
    "01998ad0-0000-7000-8000-000000000000-0".parse().expect("cursor")
}

struct Fixture {
    _home: tempfile::TempDir,
    member: Arc<CloudMember>,
    source: Arc<QuietSource>,
    provisioner: Arc<TestProvisioner>,
    worker: Worker,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_endpoint(Endpoint {
            base_url: "http://127.0.0.1:1".to_owned(),
            headers: Default::default(),
            username: "ganja".to_owned(),
            password: secrecy::SecretString::from("test-password"),
        })
        .await
    }

    async fn with_endpoint(endpoint: Endpoint) -> Self {
        let home = tempfile::tempdir().expect("fixture home");
        let name = MemberName::parse("gate-worker").expect("name");
        let lead = MemberName::lead();
        let bootstrap = Bootstrap {
            repository: "https://example.invalid/repo.git".to_owned(),
            reference: "main".to_owned(),
            branch: "workers/gate".to_owned(),
            author: Author {
                name: "worker".to_owned(),
                email: "worker@example.invalid".to_owned(),
            },
            deadline_secs: 5,
        };
        let provisioner = Arc::new(TestProvisioner::from_endpoint(endpoint));
        let worker = provisioner
            .spawn(&WorkerSpec {
                name: name.clone(),
                lead: lead.clone(),
                bootstrap: bootstrap.clone(),
                git_host: "example.invalid".to_owned(),
            })
            .await
            .expect("spawn");
        provisioner.resume(&worker).await.expect("running worker");
        let runtime = Arc::new(Runtime {
            config: ganja_core::config::CloudConfig {
                provisioner: "fake".to_owned(),
                username: "ganja".to_owned(),
                no_reply_email: "worker@example.invalid".to_owned(),
                git_host: "example.invalid".to_owned(),
                bootstrap: ganja_core::config::CloudBootstrap {
                    repository: bootstrap.repository.clone(),
                    reference: bootstrap.reference.clone(),
                    branch_prefix: "workers".to_owned(),
                },
            },
            provisioner: provisioner.clone(),
            password: None,
            deadlines: Deadlines {
                quiesce: Duration::from_millis(30),
                terminal: Duration::from_millis(30),
                ..Deadlines::default()
            },
        });
        let spec = SpawnSpec {
            name,
            team: TeamName::parse("gate-test").expect("team"),
            lead,
            root: TeamsRoot::new(home.path().join("teams")),
            backend: MemberBackend::Cloud,
            agent_type: "general".to_owned(),
            model: "fake".to_owned(),
            color: "blue".to_owned(),
            prompt: "work".to_owned(),
            cwd: home.path().to_owned(),
            plan_mode_required: false,
            parent_session_id: "lead".to_owned(),
        };
        let (exits, _) = tokio::sync::mpsc::unbounded_channel();
        let lent = Lent {
            lead_inbox: home.path().join("lead.json"),
            cancel: tokio_util::sync::CancellationToken::new(),
            dialogs: None,
            exits,
            registry: Weak::new(),
        };
        let mut member = CloudMember::new(runtime, worker.clone(), spec, lent, bootstrap);
        let source = Arc::new(QuietSource {
            gate: Arc::clone(&member.gate),
            permit: Mutex::new(None),
            held: AtomicBool::new(false),
            changed: Notify::new(),
            hold_started: Notify::new(),
            hold_release: Mutex::new(None),
            invalidations: std::sync::atomic::AtomicUsize::new(0),
        });
        Arc::get_mut(&mut member).expect("unique member").events = Some(source.clone());
        member.set_phase(Phase::Idle);
        member.gate.open_stream();
        member.gate.open_rest();
        source.connect().await.expect("connect");
        Self { _home: home, member, source, provisioner, worker }
    }

    async fn flush_notifications(&self) {
        timeout(Duration::from_secs(2), async {
            loop {
                let notifications =
                    std::mem::take(&mut *self.member.notifications.lock().expect("receipts"));
                for mut receipt in notifications {
                    let result = receipt.observe(Duration::from_secs(2)).await;
                    assert!(
                        matches!(result, Observation::Final(Value::Write(Ok(_)))),
                        "notification: {result:?}"
                    );
                }
                if self.member.outbound.outstanding() == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all lane deliveries settle before inspecting mail");
    }

    fn suspends(&self) -> usize {
        self.provisioner.suspended.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn terminal_resume_failures_deliver_exactly_one_notice_after_the_lane_drains() {
    for (kind, phase, sentence) in [
        (ProvisionErrorKind::Crashed, Phase::Crashed, "worker crashed"),
        (ProvisionErrorKind::Gone, Phase::Gone, "worker is gone"),
    ] {
        let fixture = Fixture::new().await;
        fixture.member.set_phase(Phase::Suspended);
        fixture.provisioner.owner_recover(&fixture.worker);
        *fixture.provisioner.resume_terminal.lock().expect("failure") = Some(kind);
        let control = CloudControl(Arc::clone(&fixture.member));
        assert!(control.resume_for_inspection().await.is_err());
        assert_eq!(fixture.member.phase(), phase);
        fixture.member.terminal_failure(phase, sentence).await;
        fixture.flush_notifications().await;
        assert_eq!(fixture.member.outbound.outstanding(), 0);
        let mail = mailbox::read(&fixture.member.lent.lead_inbox).expect("mail");
        assert_eq!(mail.valid.iter().filter(|message| message.text.contains(sentence)).count(), 1);
        assert!(!mail.valid.iter().any(|message| matches!(
            serde_json::from_str::<Frame>(&message.text),
            Ok(Frame::IdleNotification(_))
        )));
    }
}

#[tokio::test]
async fn dropping_the_handle_does_not_cancel_an_admitted_retirement() {
    let fixture = Fixture::new().await;
    let release = Arc::new(Notify::new());
    *fixture.source.hold_release.lock().expect("hold release") = Some(Arc::clone(&release));
    let operation = CloudControl(Arc::clone(&fixture.member))
        .admit(ControlAction::Retire)
        .expect("retirement admitted");
    fixture.source.hold_started.notified().await;
    drop(operation);
    release.notify_one();
    fixture.member.kill().await;
    assert_eq!(fixture.member.phase(), Phase::Gone);
    assert_eq!(fixture.provisioner.current(&fixture.worker), WorkerState::Gone);
}

#[tokio::test]
async fn a_second_admission_is_refused_while_one_is_in_flight() {
    let fixture = Fixture::new().await;
    let transition = fixture.member.transition.lock().await;
    let control = CloudControl(Arc::clone(&fixture.member));
    let mut first = control.admit(ControlAction::ReportExternalRevert).expect("first admission");
    assert_eq!(control.admit(ControlAction::Retire).unwrap_err(), ControlAdmissionError::Busy);
    drop(transition);
    assert_eq!(first.wait().await, ControlOutcome::Completed);
}

#[tokio::test]
async fn an_admitted_action_waiting_for_the_lock_is_cancelled_by_shutdown_without_side_effects() {
    let fixture = Fixture::new().await;
    let transition = fixture.member.transition.lock().await;
    let mut operation = CloudControl(Arc::clone(&fixture.member))
        .admit(ControlAction::ReportExternalRevert)
        .expect("report admitted");
    fixture.member.lent.cancel.cancel();
    assert_eq!(operation.wait().await, ControlOutcome::Cancelled);
    assert_eq!(fixture.source.invalidations.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.member.phase(), Phase::Idle);
    drop(transition);
}

#[tokio::test(start_paused = true)]
async fn every_control_action_has_a_finite_ceiling() {
    for (action, stalled) in [
        (ControlAction::ResumeForInspection, Some("state")),
        (ControlAction::ResumeForInspection, Some("resume")),
        (ControlAction::Retire, Some("kill")),
        (ControlAction::Retire, Some("state")),
        (ControlAction::ReportExternalRevert, None),
    ] {
        let mut fixture = Fixture::new().await;
        let member = Arc::get_mut(&mut fixture.member).expect("unique member");
        Arc::get_mut(&mut member.runtime).expect("unique runtime").deadlines = Deadlines {
            launch: Duration::from_millis(100),
            cleanup: Duration::from_millis(100),
            ..Deadlines::default()
        };
        fixture.member.set_phase(Phase::Suspended);
        fixture.provisioner.owner_recover(&fixture.worker);
        let release = stalled.map(|stalled| {
            let release = Arc::new(Notify::new());
            fixture.provisioner.holds.lock().expect("holds").insert(stalled, Arc::clone(&release));
            release
        });
        let ceiling = if action == ControlAction::Retire {
            fixture.member.runtime.deadlines.cleanup + delivery::FINAL_OBSERVATION
        } else {
            fixture.member.runtime.deadlines.launch
        };
        let start = Instant::now();
        let mut operation =
            CloudControl(Arc::clone(&fixture.member)).admit(action).expect("admitted");
        let result = timeout(ceiling + delivery::TEST_SCHEDULER_ALLOWANCE, operation.wait()).await;
        if result.is_err() {
            if let Some(release) = release {
                release.notify_one();
            }
            panic!("control operation did not return within its bound: {action:?}/{stalled:?}");
        }
        let outcome = result.expect("bounded control result");
        assert!(start.elapsed() <= ceiling, "{action:?}/{stalled:?}");
        let expected = match action {
            ControlAction::ResumeForInspection => ControlOutcome::Unsupported,
            ControlAction::Retire => ControlOutcome::UnconfirmedDeletion,
            ControlAction::ReportExternalRevert => ControlOutcome::Completed,
        };
        assert_eq!(outcome, expected, "{action:?}/{stalled:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn an_action_in_transition_finishes_within_its_budget_and_is_joined_by_kill() {
    let mut fixture = Fixture::new().await;
    let member = Arc::get_mut(&mut fixture.member).expect("unique member");
    Arc::get_mut(&mut member.runtime).expect("unique runtime").deadlines.launch =
        Duration::from_millis(100);
    fixture.provisioner.holds.lock().expect("holds").insert("state", Arc::new(Notify::new()));
    let mut operation = CloudControl(Arc::clone(&fixture.member))
        .admit(ControlAction::ResumeForInspection)
        .expect("inspection admitted");
    fixture.provisioner.called.notified().await;
    // Consume the fixture's initial resume notification, then observe the lookup.
    while !fixture.provisioner.calls.lock().expect("calls").contains(&"state") {
        fixture.provisioner.called.notified().await;
    }
    let start = Instant::now();
    fixture.member.kill().await;
    assert!(start.elapsed() <= Duration::from_millis(100));
    assert_eq!(operation.wait().await, ControlOutcome::Unsupported);
    assert!(fixture.member.control_task.lock().await.is_none());
    assert_eq!(fixture.member.phase(), Phase::Gone);
}

#[tokio::test]
async fn a_cancelled_cleanup_wait_keeps_the_owner_action_for_the_next_cleanup() {
    let fixture = Fixture::new().await;
    let release = Arc::new(Notify::new());
    *fixture.source.hold_release.lock().expect("hold release") = Some(Arc::clone(&release));
    let control = CloudControl(Arc::clone(&fixture.member));
    let mut operation =
        control.admit(ControlAction::ReportExternalRevert).expect("report admitted");
    fixture.source.hold_started.notified().await;
    {
        let cleanup = fixture.member.cleanup();
        tokio::pin!(cleanup);
        tokio::select! {
            biased;
            () = &mut cleanup => panic!("cleanup must join the held owner action"),
            () = tokio::task::yield_now() => {}
        }
        assert!(matches!(
            control.admit(ControlAction::Retire),
            Err(ControlAdmissionError::Busy | ControlAdmissionError::Cancelled)
        ));
    }
    assert!(
        fixture.member.control_task.try_lock().expect("cleanup released the slot").is_some(),
        "cancelling cleanup must not remove the owner's task"
    );
    let cleanup = fixture.member.cleanup();
    tokio::pin!(cleanup);
    tokio::select! {
        biased;
        () = &mut cleanup => panic!("the later cleanup must still join the owner action"),
        () = tokio::task::yield_now() => {}
    }
    release.notify_one();
    cleanup.await;
    assert_eq!(operation.wait().await, ControlOutcome::Completed);
    assert!(fixture.member.control_task.try_lock().expect("cleanup released the slot").is_none());
    assert_eq!(fixture.member.phase(), Phase::Gone);
}

#[tokio::test]
async fn a_reported_revert_never_calls_the_provisioner() {
    let fixture = Fixture::new().await;
    fixture.provisioner.calls.lock().expect("calls").clear();
    let mut operation = CloudControl(Arc::clone(&fixture.member))
        .admit(ControlAction::ReportExternalRevert)
        .expect("report admitted");
    assert_eq!(operation.wait().await, ControlOutcome::Completed);
    assert!(fixture.provisioner.calls.lock().expect("calls").is_empty());
    assert_eq!(fixture.source.invalidations.load(Ordering::SeqCst), 1);
    assert!(fixture.member.needs_resync.load(Ordering::SeqCst));
}

#[tokio::test]
async fn outcomes_never_carry_server_text() {
    const CANARY: &str = "endpoint-header-token-server-text-canary";
    for action in [ControlAction::ResumeForInspection, ControlAction::Retire] {
        let fixture = Fixture::new().await;
        fixture.provisioner.failures.lock().expect("failures").insert("state", CANARY);
        let mut operation =
            CloudControl(Arc::clone(&fixture.member)).admit(action).expect("admitted");
        let outcome = operation.wait().await;
        assert_eq!(
            outcome,
            if action == ControlAction::Retire {
                ControlOutcome::UnconfirmedDeletion
            } else {
                ControlOutcome::Unsupported
            }
        );
        assert!(!format!("{outcome:?} {operation:?}").contains(CANARY));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_does_not_wait_forever_for_local_notification() {
    let mut fixture = Fixture::new().await;
    let member = Arc::get_mut(&mut fixture.member).expect("unique member");
    Arc::get_mut(&mut member.runtime).expect("unique runtime").deadlines.cleanup =
        Duration::from_millis(100);
    let budget = fixture.member.runtime.deadlines.cleanup
        + delivery::FINAL_OBSERVATION
        + delivery::TEST_SCHEDULER_ALLOWANCE;
    mailbox::seed(&fixture.member.lent.lead_inbox).expect("seed inbox");
    let held = ganja_team::lock::acquire(&fixture.member.lent.lead_inbox).expect("hold inbox");
    fixture.provisioner.failures.lock().expect("failures").insert("state", "lookup");
    let mut operation = CloudControl(Arc::clone(&fixture.member))
        .admit(ControlAction::Retire)
        .expect("retirement admitted");
    let deadline = Instant::now() + budget;
    let phase_observed = timeout_at(deadline, async {
        while fixture.member.phase() != Phase::CleanupUnconfirmed {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let returned = timeout_at(deadline, operation.wait()).await;
    let transition_released = fixture.member.transition.try_lock().is_ok();
    let worker_state = fixture.provisioner.current(&fixture.worker);
    drop(held);
    let settled = timeout(budget, operation.wait()).await;
    assert!(phase_observed.is_ok(), "cleanup phase was not observed within the total budget");
    assert_eq!(worker_state, WorkerState::Gone);
    assert_eq!(settled, Ok(ControlOutcome::UnconfirmedDeletion));
    assert_eq!(
        returned,
        Ok(ControlOutcome::UnconfirmedDeletion),
        "retirement must return while the inbox is held"
    );
    assert!(transition_released, "notification delivery must not retain the transition lock");
    assert_eq!(fixture.member.phase(), Phase::CleanupUnconfirmed);
}

#[tokio::test]
async fn a_cancelled_observation_wait_can_be_resumed() {
    let fixture = Fixture::new().await;
    let transition = fixture.member.transition.lock().await;
    let mut operation = CloudControl(Arc::clone(&fixture.member))
        .admit(ControlAction::ReportExternalRevert)
        .expect("report admitted");
    tokio::select! {
        biased;
        outcome = operation.wait() => panic!("transition is still locked: {outcome:?}"),
        () = tokio::task::yield_now() => {}
    }
    drop(transition);
    assert_eq!(operation.wait().await, ControlOutcome::Completed);
    assert_eq!(operation.wait().await, ControlOutcome::Completed);
}

#[tokio::test(start_paused = true)]
async fn an_owner_action_includes_the_prior_transition_remote_budget() {
    for holder in ["launch", "resume", "quiesce"] {
        let mut fixture = Fixture::new().await;
        let member = Arc::get_mut(&mut fixture.member).expect("unique member");
        Arc::get_mut(&mut member.runtime).expect("unique runtime").deadlines = Deadlines {
            launch: Duration::from_millis(100),
            quiesce: Duration::from_millis(100),
            ..Deadlines::default()
        };
        fixture.provisioner.calls.lock().expect("calls").clear();
        let stalled = if holder == "quiesce" { "suspend" } else { "resume" };
        fixture.provisioner.holds.lock().expect("holds").insert(stalled, Arc::new(Notify::new()));
        if holder == "resume" {
            fixture.member.set_phase(Phase::Suspended);
            fixture.provisioner.owner_recover(&fixture.worker);
        }
        let transition = tokio::spawn({
            let member = Arc::clone(&fixture.member);
            async move {
                match holder {
                    "launch" => {
                        let _ = member.launch().await;
                    }
                    "resume" => {
                        let _ = member.resume().await;
                    }
                    "quiesce" => member.quiesce().await,
                    _ => unreachable!(),
                }
            }
        });
        while !fixture.provisioner.calls.lock().expect("calls").contains(&stalled) {
            fixture.provisioner.called.notified().await;
        }
        let start = Instant::now();
        let mut operation = CloudControl(Arc::clone(&fixture.member))
            .admit(ControlAction::ReportExternalRevert)
            .expect("report admitted behind transition");
        assert_eq!(operation.wait().await, ControlOutcome::Completed, "{holder}");
        assert!(start.elapsed() <= Duration::from_millis(100), "{holder}");
        transition.await.expect("transition joined");
    }
}

#[tokio::test]
async fn a_stopped_worker_can_still_be_retired() {
    let fixture = Fixture::new().await;
    fixture.member.alive.store(false, Ordering::SeqCst);
    fixture.member.set_phase(Phase::Crashed);
    let mut operation = CloudControl(Arc::clone(&fixture.member))
        .admit(ControlAction::Retire)
        .expect("stopped worker still permits deletion");
    assert_eq!(operation.wait().await, ControlOutcome::Completed);
    assert_eq!(fixture.member.phase(), Phase::Gone);
}

#[tokio::test]
async fn a_retained_member_does_not_keep_its_dead_registry_control() {
    let mut fixture = Fixture::new().await;
    let registry = Arc::new(teammate::TeammateRegistry::new(
        fixture.member.spec.root.clone(),
        fixture.member.spec.team.clone(),
        "lead",
        fixture._home.path(),
    ));
    Arc::get_mut(&mut fixture.member).expect("unique member").lent.registry =
        Arc::downgrade(&registry);
    register(&fixture.member);
    let retained = control(&registry, "gate-worker").expect("live control");
    drop(registry);
    let replacement = teammate::TeammateRegistry::new(
        fixture.member.spec.root.clone(),
        fixture.member.spec.team.clone(),
        "lead",
        fixture._home.path(),
    );
    assert!(control(&replacement, "gate-worker").is_none());
    let old = Arc::downgrade(&retained.0);
    assert!(
        CONTROLS.lock().expect("control map").values().all(|entry| !Weak::ptr_eq(entry, &old)),
        "a dead registry must be pruned even while its member lives"
    );
}

#[tokio::test]
async fn identical_disk_paths_do_not_alias_live_registry_controls() {
    let mut first = Fixture::new().await;
    let mut second = Fixture::new().await;
    Arc::get_mut(&mut second.member).expect("unique member").spec = first.member.spec.clone();
    let registry = || {
        Arc::new(teammate::TeammateRegistry::new(
            first.member.spec.root.clone(),
            first.member.spec.team.clone(),
            "lead",
            first._home.path(),
        ))
    };
    let first_registry = registry();
    let second_registry = registry();
    Arc::get_mut(&mut first.member).expect("unique member").lent.registry =
        Arc::downgrade(&first_registry);
    Arc::get_mut(&mut second.member).expect("unique member").lent.registry =
        Arc::downgrade(&second_registry);
    register(&first.member);
    register(&second.member);
    assert!(Arc::ptr_eq(
        &control(&first_registry, "gate-worker").expect("first control").0,
        &first.member,
    ));
    assert!(Arc::ptr_eq(
        &control(&second_registry, "gate-worker").expect("second control").0,
        &second.member,
    ));
    drop(second);
    assert!(control(&second_registry, "gate-worker").is_none());
    assert!(Arc::ptr_eq(
        &control(&first_registry, "gate-worker").expect("first remains").0,
        &first.member,
    ));
}

#[tokio::test]
async fn expired_controls_are_pruned_and_dead_registries_are_not_registered() {
    let mut owners = Vec::new();
    let mut expired = Vec::new();
    for _ in 0..32 {
        let mut fixture = Fixture::new().await;
        let registry = Arc::new(teammate::TeammateRegistry::new(
            fixture.member.spec.root.clone(),
            fixture.member.spec.team.clone(),
            "lead",
            fixture._home.path(),
        ));
        Arc::get_mut(&mut fixture.member).expect("unique member").lent.registry =
            Arc::downgrade(&registry);
        register(&fixture.member);
        expired.push(Arc::downgrade(&fixture.member));
        owners.push(registry);
    }
    let mut fixture = Fixture::new().await;
    register(&fixture.member);
    let dead_member = Arc::downgrade(&fixture.member);
    assert!(CONTROLS.lock().expect("control map").values().all(|entry| {
        !Weak::ptr_eq(entry, &dead_member) && expired.iter().all(|old| !Weak::ptr_eq(entry, old))
    }));
    drop(dead_member);
    let dead_registry = Arc::new(teammate::TeammateRegistry::new(
        fixture.member.spec.root.clone(),
        fixture.member.spec.team.clone(),
        "lead",
        fixture._home.path(),
    ));
    Arc::get_mut(&mut fixture.member).expect("unique member").lent.registry =
        Arc::downgrade(&dead_registry);
    drop(dead_registry);
    register(&fixture.member);
    let dead_member = Arc::downgrade(&fixture.member);
    assert!(
        CONTROLS
            .lock()
            .expect("control map")
            .values()
            .all(|entry| !Weak::ptr_eq(entry, &dead_member))
    );
    assert!(control(&owners[0], "gate-worker").is_none());
    let mut lookup_fixture = Fixture::new().await;
    Arc::get_mut(&mut lookup_fixture.member).expect("unique member").lent.registry =
        Arc::downgrade(&owners[0]);
    register(&lookup_fixture.member);
    let expired_lookup = Arc::downgrade(&lookup_fixture.member);
    drop(lookup_fixture);
    assert!(control(&owners[0], "invalid name").is_none());
    assert!(
        CONTROLS
            .lock()
            .expect("control map")
            .values()
            .all(|entry| !Weak::ptr_eq(entry, &expired_lookup))
    );
}

#[tokio::test]
async fn same_named_members_in_different_teams_keep_independent_controls() {
    let mut first = Fixture::new().await;
    let mut second = Fixture::new().await;
    Arc::get_mut(&mut second.member).expect("unique member").spec.team =
        TeamName::parse("other-team").expect("team");
    let registry = |fixture: &Fixture| {
        Arc::new(teammate::TeammateRegistry::new(
            fixture.member.spec.root.clone(),
            fixture.member.spec.team.clone(),
            "lead",
            fixture._home.path(),
        ))
    };
    let first_registry = registry(&first);
    let second_registry = registry(&second);
    Arc::get_mut(&mut first.member).expect("unique member").lent.registry =
        Arc::downgrade(&first_registry);
    Arc::get_mut(&mut second.member).expect("unique member").lent.registry =
        Arc::downgrade(&second_registry);
    register(&first.member);
    register(&second.member);
    let first_control = control(&first_registry, "gate-worker").expect("first control");
    assert!(Arc::ptr_eq(&first_control.0, &first.member));
    first_control.report_external_revert().await;
    assert_eq!(first.member.phase(), Phase::Suspended);
    assert_eq!(second.member.phase(), Phase::Idle);
    let second_control = control(&second_registry, "gate-worker").expect("second control");
    assert!(Arc::ptr_eq(&second_control.0, &second.member));
    second_control.retire().await.expect("second worker retired");
    assert_eq!(second.member.phase(), Phase::Gone);
    assert_ne!(first.member.phase(), Phase::Gone);
    drop(second_control);
    drop(second);
    assert!(
        control(&second_registry, "gate-worker").is_none(),
        "second registry has no live member"
    );
    assert!(Arc::ptr_eq(
        &control(&first_registry, "gate-worker").expect("first remains").0,
        &first.member
    ));
}

#[tokio::test]
async fn worker_endpoints_are_origins_without_path_prefixes() {
    for (base_url, accepted) in [
        ("https://worker.example", true),
        ("https://worker.example/", true),
        ("https://worker.example/prefix", false),
        ("https://worker.example/.", false),
        ("https://worker.example/%2e", false),
        ("https://worker.example//", false),
    ] {
        let fixture = Fixture::with_endpoint(Endpoint {
            base_url: base_url.to_owned(),
            headers: http::HeaderMap::new(),
            username: "ganja".to_owned(),
            password: secrecy::SecretString::from("test-password"),
        })
        .await;
        assert_eq!(fixture.member.client.is_some(), accepted, "{base_url}");
    }
}

#[tokio::test]
async fn worker_clients_require_trusted_endpoints_and_separate_auth_headers() {
    let cases = [
        ("https://worker.example", None, true),
        ("http://127.0.0.1:12345", Some("ate-target-actor"), true),
        ("http://127.12.34.56:80", None, true),
        ("http://[::1]:12345", None, true),
        ("http://localhost:12345", None, true),
        ("http://worker.example", None, false),
        ("http://192.0.2.1", None, false),
        ("http://[::2]", None, false),
        ("https://user@worker.example", None, false),
        ("https://@worker.example", None, false),
        ("https://worker.example?route=other", None, false),
        ("https://worker.example#route", None, false),
        ("http://127.0.0.1:12345", Some("Authorization"), false),
        ("http://127.0.0.1:12345", Some("aUtHoRiZaTiOn"), false),
        ("http://127.0.0.1:12345", Some("Proxy-Authorization"), false),
    ];
    for (base_url, header, accepted) in cases {
        let mut headers = http::HeaderMap::new();
        if let Some(header) = header {
            headers.insert(
                http::HeaderName::from_bytes(header.as_bytes()).expect("header name"),
                http::HeaderValue::from_static("space/actor"),
            );
        }
        let fixture = Fixture::with_endpoint(Endpoint {
            base_url: base_url.to_owned(),
            headers,
            username: "ganja".to_owned(),
            password: secrecy::SecretString::from("test-password"),
        })
        .await;
        assert_eq!(fixture.member.client.is_some(), accepted, "{base_url}, {header:?}");
    }
}

#[tokio::test]
async fn an_already_suspended_result_invalidates_history_without_reconnecting() {
    let fixture = Fixture::new().await;
    fixture.provisioner.owner_recover(&fixture.worker);
    fixture.member.quiesce().await;
    assert_eq!(fixture.member.phase(), Phase::Suspended);
    assert_eq!(fixture.source.invalidations.load(Ordering::SeqCst), 1);
    assert!(fixture.member.needs_resync.load(Ordering::SeqCst));
    assert!(!fixture.member.gate.state().stream_open);
    assert!(!fixture.member.gate.state().rest_open);
    assert_eq!(fixture.provisioner.current(&fixture.worker), WorkerState::Suspended);
}

#[tokio::test]
async fn inspection_of_an_external_suspend_invalidates_without_waking_the_worker() {
    let fixture = Fixture::new().await;
    fixture.provisioner.owner_recover(&fixture.worker);
    let control = CloudControl(fixture.member.clone());
    assert!(control.resume_for_inspection().await.is_err());
    assert_eq!(fixture.provisioner.current(&fixture.worker), WorkerState::Suspended);
    assert_eq!(fixture.source.invalidations.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.member.phase(), Phase::Suspended);
    assert!(fixture.member.needs_resync.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_crashed_state_followed_by_external_recovery_invalidates_before_reconnect() {
    let fixture = Fixture::new().await;
    let control = CloudControl(fixture.member.clone());
    *fixture.provisioner.state.lock().expect("state") = WorkerState::Crashed;
    assert!(control.resume_for_inspection().await.is_err());
    assert!(fixture.member.observed_crash.load(Ordering::SeqCst));
    assert!(!fixture.member.gate.state().stream_open);
    *fixture.provisioner.state.lock().expect("state") = WorkerState::Running;
    assert!(control.resume_for_inspection().await.is_err());
    assert_eq!(fixture.source.invalidations.load(Ordering::SeqCst), 1);
    assert!(fixture.member.needs_resync.load(Ordering::SeqCst));
    assert!(!fixture.member.observed_crash.load(Ordering::SeqCst));
    assert_eq!(fixture.member.phase(), Phase::RecoveryRequired);
}

#[tokio::test]
async fn an_unexpected_resumed_transition_keeps_uncertainty_and_requires_new_inspection() {
    let fixture = Fixture::new().await;
    fixture.member.gate.admit(Door::Rest).expect("open").finish(AttemptOutcome::Uncertain);
    fixture.member.set_phase(Phase::RecoveryRequired);
    fixture.provisioner.owner_recover(&fixture.worker);
    assert!(fixture.member.resume().await.is_err());
    assert!(fixture.member.needs_resync.load(Ordering::SeqCst));
    assert!(fixture.member.gate.state().uncertain);
    assert!(!fixture.member.gate.state().stream_open);
    assert_eq!(fixture.member.phase(), Phase::RecoveryRequired);
    assert_eq!(
        fixture
            .member
            .recent
            .lock()
            .expect("recent")
            .iter()
            .filter(|row| row.contains("unexpected suspended"))
            .count(),
        1
    );
}

#[tokio::test]
async fn an_absent_initial_envelope_does_not_wait_forever_or_claim_idle() {
    let fixture = Fixture::new().await;
    *fixture.member.session.lock().expect("session") = Some(SessionId::ascending());
    let result =
        timeout(Duration::from_secs(1), fixture.member.wait_for_terminal(None, None, false))
            .await
            .expect("terminal reconciliation is bounded");
    assert_eq!(result, Err("terminal state unavailable"));
    assert!(fixture.member.terminal.lock().expect("terminal decisions").is_empty());
    assert!(!fixture.member.lent.lead_inbox.exists(), "no idle notification was written");
}

#[tokio::test]
async fn a_healthy_stream_is_actively_ended_before_suspend() {
    let fixture = Fixture::new().await;
    let source = fixture.source.clone();
    let reader = tokio::spawn(async move { source.next().await });
    fixture.member.quiesce().await;
    assert!(reader.await.expect("reader joined").is_none());
    assert_eq!(fixture.member.gate.state().streams, 0);
    assert_eq!(fixture.provisioner.current(&fixture.worker), WorkerState::Suspended);
    assert_eq!(fixture.suspends(), 1);
}

#[tokio::test]
async fn suspend_waits_for_stream_hold_acknowledgement_with_both_doors_closed() {
    let fixture = Fixture::new().await;
    let release = Arc::new(Notify::new());
    *fixture.source.hold_release.lock().expect("hold release") = Some(release.clone());
    let member = fixture.member.clone();
    let quiesce = tokio::spawn(async move { member.quiesce().await });
    timeout(Duration::from_secs(1), fixture.source.hold_started.notified())
        .await
        .expect("hold requested");
    assert_eq!(fixture.suspends(), 0, "suspend cannot precede hold acknowledgement");
    let state = fixture.member.gate.state();
    assert!(!state.stream_open && !state.rest_open);
    assert_eq!(state.streams, 1, "the established stream still holds its permit");
    assert!(fixture.source.connect().await.expect("closed admission").is_none());
    release.notify_one();
    quiesce.await.expect("quiesce joined");
    assert_eq!(fixture.member.gate.state().streams, 0);
    assert_eq!(fixture.suspends(), 1);
}

#[tokio::test]
async fn an_inflight_rest_request_completes_before_suspend() {
    let fixture = Fixture::new().await;
    let permit = fixture.member.gate.admit(Door::Rest).expect("REST open");
    let member = fixture.member.clone();
    let quiesce = tokio::spawn(async move { member.quiesce().await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !fixture.source.held.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("hold acknowledged");
    assert_eq!(fixture.suspends(), 0);
    assert!(fixture.member.gate.admit(Door::Stream).is_none());
    assert!(fixture.member.gate.admit(Door::Rest).is_none());
    permit.finish(AttemptOutcome::Serve);
    quiesce.await.expect("quiesce joined");
    assert_eq!(fixture.suspends(), 1);
}

#[tokio::test]
async fn a_rest_drain_deadline_defers_without_latching_uncertainty() {
    let fixture = Fixture::new().await;
    let permit = fixture.member.gate.admit(Door::Rest).expect("REST open");
    fixture.member.quiesce().await;
    assert_eq!(fixture.suspends(), 0);
    assert_eq!(fixture.member.phase(), Phase::Idle);
    assert!(!fixture.member.gate.state().uncertain);
    assert!(fixture.member.gate.state().rest_open);
    permit.finish(AttemptOutcome::Serve);
    fixture.member.quiesce().await;
    assert_eq!(fixture.suspends(), 1);
}

#[tokio::test]
async fn an_unproven_rest_attempt_stays_uncertain_after_owner_recovery() {
    let fixture = Fixture::new().await;
    fixture.member.gate.admit(Door::Rest).expect("REST open").finish(AttemptOutcome::Uncertain);
    fixture.member.quiesce().await;
    fixture.provisioner.owner_recover(&fixture.worker);
    fixture.provisioner.resume(&fixture.worker).await.expect("owner resumes");
    fixture.member.quiesce().await;
    assert_eq!(fixture.suspends(), 0);
    assert!(fixture.member.gate.state().uncertain);
    fixture.flush_notifications().await;
    let mail = mailbox::read(&fixture.member.lent.lead_inbox).expect("owner mail");
    assert_eq!(
        mail.valid.iter().filter(|message| message.text.contains("earlier request")).count(),
        1
    );
}
