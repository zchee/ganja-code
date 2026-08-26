use super::*;

fn unset() -> ResolvedInbound {
    ResolvedInbound::new(None)
}

fn explicit(policy: InboundPolicy) -> ResolvedInbound {
    ResolvedInbound::new(Some((policy, PolicySource::Global)))
}

fn qualified<'a>(sender: &'a str, body: &'a str) -> Origin<'a> {
    Origin {
        tier: Tier::Qualified { sender },
        hop_chain: &[],
        own_marker: None,
        body,
    }
}

fn message(text: &str) -> MailboxMessage {
    MailboxMessage::new("stranger", text, "2026-08-26T00:00:00Z")
}

fn drained(rx: &mut mpsc::UnboundedReceiver<HoldTransition>) -> Vec<HoldTransition> {
    let mut transitions = Vec::new();
    while let Ok(transition) = rx.try_recv() {
        transitions.push(transition);
    }
    transitions
}

// AC-5: the full matrix under `true`.
#[test]
fn the_full_matrix_reproduces_all_eight_rows() {
    use ReceiverClass as R;
    use SenderClass as S;
    let decide = |r, s| decide_unset(r, s, false, true);

    assert_eq!(
        decide(Some(R::Prompting), Some(S::Prompting)),
        Verdict::Accept
    );
    assert_eq!(
        decide(Some(R::Prompting), Some(S::Bypass)),
        Verdict::Hold(HoldCause::ModeMismatch)
    );
    assert_eq!(decide(Some(R::Bypass), Some(S::Bypass)), Verdict::Accept);
    assert_eq!(
        decide(Some(R::Bypass), Some(S::Prompting)),
        Verdict::Hold(HoldCause::ModeMismatch)
    );
    assert_eq!(decide(Some(R::Prompting), None), Verdict::Accept);
    assert_eq!(
        decide(Some(R::Bypass), None),
        Verdict::Hold(HoldCause::NoModeAsserted)
    );
    assert_eq!(decide(None, None), Verdict::Hold(HoldCause::ModeUnknown));
    assert_eq!(
        decide_unset(Some(R::Bypass), None, true, true),
        Verdict::Accept
    );
}

// AC-5: under `false` the sender class is never consulted — every sender
// value yields the collapsed verdict, including the one that would
// accept under the full matrix.
#[test]
fn the_collapsed_path_never_reads_the_sender() {
    use ReceiverClass as R;
    use SenderClass as S;
    for sender in [None, Some(S::Prompting), Some(S::Bypass)] {
        assert_eq!(
            decide_unset(Some(R::Prompting), sender, false, false),
            Verdict::Accept
        );
        assert_eq!(
            decide_unset(Some(R::Bypass), sender, false, false),
            Verdict::Hold(HoldCause::NoModeAsserted)
        );
    }
    // The proof point: bypass/bypass accepts under the full matrix, and
    // still holds under the collapsed path.
    assert_eq!(
        decide_unset(Some(R::Bypass), Some(S::Bypass), false, true),
        Verdict::Accept
    );
    assert_eq!(
        decide_unset(Some(R::Bypass), Some(S::Bypass), false, false),
        Verdict::Hold(HoldCause::NoModeAsserted)
    );
}

#[test]
fn an_unreadable_receiver_holds_even_a_self_sent_message() {
    assert_eq!(
        decide_unset(None, None, true, false),
        Verdict::Hold(HoldCause::ModeUnknown)
    );
    assert_eq!(
        decide_unset(None, None, true, true),
        Verdict::Hold(HoldCause::ModeUnknown)
    );
}

#[test]
fn self_sent_accepts_on_the_collapsed_path_too() {
    assert_eq!(
        decide_unset(Some(ReceiverClass::Bypass), None, true, false),
        Verdict::Accept
    );
}

// AC-6: the explicit branch comes first, so nothing the matrix reads —
// `self_sent` included — can override a configured value.
#[test]
fn an_explicit_policy_wins_over_parity_even_when_self_sent() {
    let refuse = explicit(InboundPolicy::Refuse);
    assert_eq!(
        refuse.decide(Some(ReceiverClass::Prompting), None, true, false),
        Verdict::Refuse(RefuseCause::Explicit {
            source: PolicySource::Global
        })
    );
    let hold = explicit(InboundPolicy::Hold);
    assert_eq!(
        hold.decide(Some(ReceiverClass::Prompting), None, true, false),
        Verdict::Hold(HoldCause::Explicit {
            source: PolicySource::Global
        })
    );
    // Explicit wins over the fail-closed arm as well: a configured
    // accept delivers even where the mode is unreadable.
    let accept = explicit(InboundPolicy::Accept);
    assert_eq!(accept.decide(None, None, false, false), Verdict::Accept);
}

// AC-7, the core half: classification is total, and the fail-closed arm
// is exercised directly through the resolver's `None`.
#[test]
fn receiver_classification_is_total_including_the_mode_unknown_arm() {
    assert_eq!(
        classify_receiver(PermissionMode::Ask, false),
        ReceiverClass::Prompting
    );
    assert_eq!(
        classify_receiver(PermissionMode::Ask, true),
        ReceiverClass::Bypass
    );
    assert_eq!(
        classify_receiver(PermissionMode::Bypass, false),
        ReceiverClass::Bypass
    );
    assert_eq!(
        classify_receiver(PermissionMode::Bypass, true),
        ReceiverClass::Bypass
    );
    assert_eq!(
        unset().decide(None, None, false, false),
        Verdict::Hold(HoldCause::ModeUnknown)
    );
}

// AC-23: the four reasons are `admit`'s whole vocabulary, and the queue
// cap is a separate test that drops the 51st at the door while `admit`
// itself still says yes.
#[tokio::test(start_paused = true)]
async fn the_queue_cap_is_separate_from_admit_and_drops_the_fifty_first() {
    let (gate, _drain) = Inbound::new(explicit(InboundPolicy::Accept), DialogExpiry::default());
    for i in 0..MAX_QUEUED_PEERS {
        gate.admit_identity(mailbox::identity(&message(&format!("m{i}"))));
    }
    {
        let mut state = gate.lock();
        assert!(queue_full(state.admitted.len()));
        // The guard still admits: the cap is not one of its reasons.
        assert_eq!(state.guard.admit(&qualified("w1@t", "fresh body")), Ok(()));
    }
    assert_eq!(
        gate.admit_socket(Some(ReceiverClass::Prompting), "w2@t", "the 51st", None),
        SocketAdmission::Silent(DropReason::QueueFull)
    );
    assert!(!queue_full(MAX_QUEUED_PEERS - 1));
}

// AC-24, the bucket half: a 30-burst drains it, the 31st drops, and two
// paused seconds refill exactly one admission.
#[tokio::test(start_paused = true)]
async fn the_thirty_first_message_in_a_burst_is_rate_limited_and_refills_after_two_seconds() {
    let mut guard = PeerGuard::new();
    for i in 0..30 {
        assert_eq!(guard.admit(&qualified("w1@t", &format!("m{i}"))), Ok(()));
    }
    assert_eq!(
        guard.admit(&qualified("w1@t", "m30")),
        Err(Dropped::RateLimited)
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(guard.admit(&qualified("w1@t", "m31")), Ok(()));
    assert_eq!(
        guard.admit(&qualified("w1@t", "m32")),
        Err(Dropped::RateLimited)
    );
}

// AC-24, the dedup half: same key and body inside the window drops,
// another key admits, and the window's edge releases.
#[tokio::test(start_paused = true)]
async fn an_identical_body_within_thirty_seconds_is_a_duplicate_and_another_key_is_not() {
    let mut guard = PeerGuard::new();
    assert_eq!(guard.admit(&qualified("w1@t", "same body")), Ok(()));
    assert_eq!(
        guard.admit(&qualified("w1@t", "same body")),
        Err(Dropped::Duplicate)
    );
    assert_eq!(guard.admit(&qualified("w2@t", "same body")), Ok(()));
    tokio::time::advance(DEDUP_WINDOW).await;
    assert_eq!(guard.admit(&qualified("w1@t", "same body")), Ok(()));
}

// AC-25: synthetic chains — the wire carries none, the logic is total.
#[tokio::test(start_paused = true)]
async fn a_twenty_nine_entry_chain_is_hop_runaway_and_twenty_eight_passes() {
    let mut guard = PeerGuard::new();
    let long: Vec<String> = (0..29).map(|i| format!("s{i}")).collect();
    let runaway = Origin {
        tier: Tier::Qualified { sender: "w1@t" },
        hop_chain: &long,
        own_marker: None,
        body: "b1",
    };
    assert_eq!(guard.admit(&runaway), Err(Dropped::HopRunaway));
    let edge = &long[..28];
    let passes = Origin {
        tier: Tier::Qualified { sender: "w1@t" },
        hop_chain: edge,
        own_marker: None,
        body: "b2",
    };
    assert_eq!(guard.admit(&passes), Ok(()));
}

#[tokio::test(start_paused = true)]
async fn an_eleventh_own_marker_is_hop_loop_and_ten_pass() {
    let mut guard = PeerGuard::new();
    let eleven: Vec<String> = (0..11).map(|_| "me".to_owned()).collect();
    let looped = Origin {
        tier: Tier::Qualified { sender: "w1@t" },
        hop_chain: &eleven,
        own_marker: Some("me"),
        body: "b1",
    };
    assert_eq!(guard.admit(&looped), Err(Dropped::HopLoop));
    let ten = &eleven[..10];
    let passes = Origin {
        tier: Tier::Qualified { sender: "w1@t" },
        hop_chain: ten,
        own_marker: Some("me"),
        body: "b2",
    };
    assert_eq!(guard.admit(&passes), Ok(()));
}

// AC-26: no usable provenance means no bucket and no dedup — identical
// rapid entries all admit — while the door still queue-caps.
#[tokio::test(start_paused = true)]
async fn the_demoted_tier_skips_bucket_and_dedup_but_is_still_queue_capped() {
    let mut guard = PeerGuard::new();
    let unidentified = Origin {
        tier: Tier::Unidentified,
        hop_chain: &[],
        own_marker: None,
        body: "same",
    };
    for _ in 0..40 {
        assert_eq!(guard.admit(&unidentified), Ok(()));
    }

    let (gate, _drain) = Inbound::new(explicit(InboundPolicy::Accept), DialogExpiry::default());
    for i in 0..MAX_QUEUED_PEERS {
        gate.admit_identity(mailbox::identity(&message(&format!("m{i}"))));
    }
    assert_eq!(
        gate.admit_mailbox(Some(ReceiverClass::Prompting), &message("the 51st")),
        MailboxAdmission::Drop(DropReason::QueueFull)
    );
}

// The 256-sender bound evicts least-recently-used state (M5): the
// evicted sender forgets its dedup window, a retained one remembers.
#[tokio::test(start_paused = true)]
async fn the_two_hundred_fifty_seventh_sender_evicts_the_least_recently_used() {
    let mut guard = PeerGuard::new();
    for i in 0..TRACKED_SENDERS {
        assert_eq!(guard.admit(&qualified(&format!("s{i}@t"), "b")), Ok(()));
    }
    // Touch s0 again — a duplicate drop still refreshes recency, so s1
    // becomes the least recently used.
    assert_eq!(
        guard.admit(&qualified("s0@t", "b")),
        Err(Dropped::Duplicate)
    );
    assert_eq!(guard.admit(&qualified("new@t", "b")), Ok(()));
    assert_eq!(guard.senders.len(), TRACKED_SENDERS);
    // s1's state went with the eviction: its identical body admits
    // afresh (and that re-insertion evicts the next LRU entry in turn) —
    // while s0, the recently touched entry the order protected, still
    // remembers the body and drops it.
    assert_eq!(guard.admit(&qualified("s1@t", "b")), Ok(()));
    assert_eq!(
        guard.admit(&qualified("s0@t", "b")),
        Err(Dropped::Duplicate)
    );
    assert_eq!(guard.senders.len(), TRACKED_SENDERS);
}

// AC-12, the buffer half: the 101st hold evicts the oldest, settled
// `expired` before the newcomer appends, and a mailbox-door victim's
// prune surfaces as data.
#[tokio::test(start_paused = true)]
async fn the_hundred_and_first_hold_evicts_the_oldest_settled_expired() {
    let (gate, mut drain) = Inbound::new(explicit(InboundPolicy::Hold), DialogExpiry::default());
    let first = message("the oldest");
    let first_identity = mailbox::identity(&first);
    assert!(matches!(
        gate.admit_mailbox(Some(ReceiverClass::Prompting), &first),
        MailboxAdmission::Held {
            evicted_prune: None,
            ..
        }
    ));
    for i in 1..HELD_CAP {
        assert!(matches!(
            gate.admit_socket(
                Some(ReceiverClass::Prompting),
                "w@t",
                &format!("m{i}"),
                None
            ),
            SocketAdmission::Held {
                evicted_prune: None,
                ..
            }
        ));
    }
    let oldest_id = gate.held_messages()[0].id.clone();

    let overflow = gate.admit_socket(Some(ReceiverClass::Prompting), "w@t", "the 101st", None);
    let SocketAdmission::Held { evicted_prune, .. } = overflow else {
        panic!("the overflowing hold still holds: {overflow:?}");
    };
    assert_eq!(evicted_prune, Some(first_identity.clone()));
    assert_eq!(gate.held_messages().len(), HELD_CAP);
    assert_eq!(gate.disposition(&first_identity), PassDisposition::Classify);

    let transitions = drained(&mut drain);
    let tail = &transitions[transitions.len() - 2..];
    assert!(
        matches!(&tail[0], HoldTransition::Settled { id, outcome: HeldOutcome::Expired } if *id == oldest_id),
        "the eviction settles before the newcomer appends: {tail:?}"
    );
    assert!(matches!(&tail[1], HoldTransition::Held { .. }));
}

// AC-13, the buffer half: a deadline exists exactly for the parity
// causes.
#[test]
fn only_the_parity_causes_earn_a_deadline() {
    let expiry = DialogExpiry::default();
    assert_eq!(
        deadline_for(HoldCause::ModeMismatch, expiry),
        Some(Duration::from_secs(300))
    );
    assert_eq!(
        deadline_for(HoldCause::NoModeAsserted, expiry),
        Some(Duration::from_secs(300))
    );
    assert_eq!(
        deadline_for(
            HoldCause::Explicit {
                source: PolicySource::Project
            },
            expiry
        ),
        None
    );
    assert_eq!(deadline_for(HoldCause::ModeUnknown, expiry), None);
    // `never` is no deadline even for a parity cause.
    assert_eq!(
        deadline_for(HoldCause::NoModeAsserted, DialogExpiry::Never),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn an_explicit_hold_installs_no_timer_and_a_parity_hold_counts_down() {
    let (explicit_gate, mut explicit_drain) =
        Inbound::new(explicit(InboundPolicy::Hold), DialogExpiry::default());
    assert!(matches!(
        explicit_gate.admit_socket(Some(ReceiverClass::Bypass), "w@t", "held", None),
        SocketAdmission::Held {
            cause: HoldCause::Explicit { .. },
            ..
        }
    ));
    assert_eq!(explicit_gate.held_messages()[0].expires_in, None);
    let transitions = drained(&mut explicit_drain);
    assert!(matches!(
        &transitions[0],
        HoldTransition::Held {
            expires_in_ms: None,
            ..
        }
    ));

    let (parity_gate, mut parity_drain) = Inbound::new(unset(), DialogExpiry::default());
    assert!(matches!(
        parity_gate.admit_socket(Some(ReceiverClass::Bypass), "w@t", "held", None),
        SocketAdmission::Held {
            cause: HoldCause::NoModeAsserted,
            ..
        }
    ));
    assert_eq!(
        parity_gate.held_messages()[0].expires_in,
        Some(Duration::from_secs(300))
    );
    let transitions = drained(&mut parity_drain);
    assert!(matches!(
        &transitions[0],
        HoldTransition::Held {
            expires_in_ms: Some(300_000),
            ..
        }
    ));

    // A mode-unknown hold sits deadline-free like an explicit one.
    assert!(matches!(
        parity_gate.admit_socket(None, "w@t", "unknown", None),
        SocketAdmission::Held {
            cause: HoldCause::ModeUnknown,
            ..
        }
    ));
    assert_eq!(parity_gate.held_messages()[1].expires_in, None);
}

// Pre-mortem 3: two settlement paths contend in the same paused tick and
// the first wins — the loser finds the id claimed or gone and no-ops.
#[tokio::test(start_paused = true)]
async fn the_first_settler_wins_when_approve_and_expiry_race() {
    let (gate, mut drain) = Inbound::new(unset(), DialogExpiry::default());
    assert!(matches!(
        gate.admit_socket(Some(ReceiverClass::Bypass), "w@t", "raced", None),
        SocketAdmission::Held { .. }
    ));
    let id = gate.held_messages()[0].id.clone();

    let released = gate.release(&id);
    assert!(
        matches!(released, Some(Settlement::Deliver(_))),
        "{released:?}"
    );
    assert_eq!(gate.expire(&id), None);
    assert_eq!(gate.deny(&id), None);

    let settlements: Vec<_> = drained(&mut drain)
        .into_iter()
        .filter(|transition| matches!(transition, HoldTransition::Settled { .. }))
        .collect();
    assert_eq!(
        settlements,
        vec![HoldTransition::Settled {
            id,
            outcome: HeldOutcome::Delivered
        }]
    );
}

// H2: a mailbox-door drop claims the record and waits for its prune; a
// failed prune re-holds, a landed one finishes with the claimed outcome.
#[tokio::test(start_paused = true)]
async fn a_mailbox_deny_prunes_first_and_a_failed_prune_re_holds() {
    let (gate, mut drain) = Inbound::new(explicit(InboundPolicy::Hold), DialogExpiry::default());
    let held = message("deny me");
    let identity = mailbox::identity(&held);
    assert!(matches!(
        gate.admit_mailbox(Some(ReceiverClass::Prompting), &held),
        MailboxAdmission::Held { .. }
    ));
    let id = gate.held_messages()[0].id.clone();

    let step = gate.deny(&id);
    assert_eq!(
        step,
        Some(Settlement::PruneFirst {
            identity: identity.clone()
        })
    );
    // Claimed: still held, still indexed, and no second settler lands.
    assert_eq!(gate.held_messages().len(), 1);
    assert_eq!(gate.disposition(&identity), PassDisposition::Skip);
    assert_eq!(gate.deny(&id), None);
    assert_eq!(gate.release(&id), None);

    gate.prune_failed(&id);
    // Re-held and retryable: the same deny claims again.
    assert_eq!(
        gate.deny(&id),
        Some(Settlement::PruneFirst {
            identity: identity.clone()
        })
    );
    assert_eq!(gate.pruned(&id), Some(HeldOutcome::Denied));
    assert!(gate.held_messages().is_empty());
    assert_eq!(gate.disposition(&identity), PassDisposition::Classify);
    let settled: Vec<_> = drained(&mut drain)
        .into_iter()
        .filter(|transition| matches!(transition, HoldTransition::Settled { .. }))
        .collect();
    assert_eq!(
        settled,
        vec![HoldTransition::Settled {
            id,
            outcome: HeldOutcome::Denied
        }]
    );
}

// The release re-check: an approval cannot override a policy that has
// since become refuse — the socket record settles denied, and nothing
// says deliver.
#[tokio::test(start_paused = true)]
async fn a_release_re_checks_policy_and_a_now_refusing_one_denies() {
    let (gate, mut drain) = Inbound::new(unset(), DialogExpiry::default());
    assert!(matches!(
        gate.admit_socket(Some(ReceiverClass::Bypass), "w@t", "approved late", None),
        SocketAdmission::Held { .. }
    ));
    let id = gate.held_messages()[0].id.clone();

    gate.replace_policy(explicit(InboundPolicy::Refuse));
    assert_eq!(
        gate.release(&id),
        Some(Settlement::Done(HeldOutcome::Denied))
    );
    assert!(gate.held_messages().is_empty());
    let last = drained(&mut drain).pop();
    assert_eq!(
        last,
        Some(HoldTransition::Settled {
            id,
            outcome: HeldOutcome::Denied
        })
    );
}

// H1: a mailbox-door release moves the identity held-index → admitted
// carrying the hold-time summary snapshot — what the pass delivers, not
// whatever the durable entry says by then.
#[tokio::test(start_paused = true)]
async fn a_mailbox_release_delivers_the_hold_time_summary_snapshot() {
    let (gate, _drain) = Inbound::new(explicit(InboundPolicy::Hold), DialogExpiry::default());
    let mut held = message("release me");
    held.summary = Some("the reviewed summary".to_owned());
    let identity = mailbox::identity(&held);
    assert!(matches!(
        gate.admit_mailbox(Some(ReceiverClass::Prompting), &held),
        MailboxAdmission::Held { .. }
    ));
    let id = gate.held_messages()[0].id.clone();

    assert_eq!(
        gate.release(&id),
        Some(Settlement::Done(HeldOutcome::Delivered))
    );
    assert_eq!(
        gate.disposition(&identity),
        PassDisposition::DeliverReviewed {
            summary: Some(RedactedText::from("the reviewed summary".to_owned()))
        }
    );
}

// A socket-door release hands back the write, and the identity the write
// mints joins the admitted set through the caller's hands (M6).
#[tokio::test(start_paused = true)]
async fn a_socket_release_hands_back_the_write_and_admits_what_it_minted() {
    let (gate, _drain) = Inbound::new(unset(), DialogExpiry::default());
    assert!(matches!(
        gate.admit_socket(Some(ReceiverClass::Bypass), "w@t", "write me", Some("s")),
        SocketAdmission::Held { .. }
    ));
    let id = gate.held_messages()[0].id.clone();

    let Some(Settlement::Deliver(released)) = gate.release(&id) else {
        panic!("a socket release is a write");
    };
    assert_eq!(released.from, "w@t");
    assert_eq!(released.text, "write me");
    assert_eq!(released.summary.as_deref(), Some("s"));

    let minted = mailbox::identity(&MailboxMessage::new(
        released.from,
        released.text,
        "2026-08-26T00:00:01Z",
    ));
    gate.admit_identity(minted.clone());
    assert_eq!(gate.disposition(&minted), PassDisposition::Deliver);
}

// AC-16's core shape: a mode change re-decides every hold under its own
// origin — parity holds release, explicit holds stay exactly as held.
#[tokio::test(start_paused = true)]
async fn a_mode_change_reevaluation_releases_parity_holds_and_leaves_explicit_ones() {
    let (gate, _drain) = Inbound::new(unset(), DialogExpiry::default());
    assert!(matches!(
        gate.admit_socket(Some(ReceiverClass::Bypass), "w@t", "socket held", None),
        SocketAdmission::Held { .. }
    ));
    let held = message("mailbox held");
    let identity = mailbox::identity(&held);
    assert!(matches!(
        gate.admit_mailbox(Some(ReceiverClass::Bypass), &held),
        MailboxAdmission::Held { .. }
    ));

    let actions = gate.reevaluate(Some(ReceiverClass::Prompting));
    assert_eq!(actions.len(), 2);
    assert!(
        matches!(&actions[0].settlement, Settlement::Deliver(released) if released.text == "socket held")
    );
    assert_eq!(
        actions[1].settlement,
        Settlement::Done(HeldOutcome::Delivered)
    );
    assert!(gate.held_messages().is_empty());
    assert!(matches!(
        gate.disposition(&identity),
        PassDisposition::DeliverReviewed { .. }
    ));

    let (explicit_gate, _drain) =
        Inbound::new(explicit(InboundPolicy::Hold), DialogExpiry::default());
    assert!(matches!(
        explicit_gate.admit_socket(Some(ReceiverClass::Bypass), "w@t", "stays", None),
        SocketAdmission::Held { .. }
    ));
    let before = explicit_gate.held_messages();
    assert!(
        explicit_gate
            .reevaluate(Some(ReceiverClass::Prompting))
            .is_empty()
    );
    assert_eq!(explicit_gate.held_messages(), before);
}

// Reconciliation: a held identity gone from the inbox settles expired —
// a review offer cannot outlive the bytes it reviews — and a consumed
// admitted identity leaves the set.
#[tokio::test(start_paused = true)]
async fn reconcile_expires_vanished_holds_and_forgets_consumed_admissions() {
    let (gate, mut drain) = Inbound::new(explicit(InboundPolicy::Hold), DialogExpiry::default());
    let held = message("about to vanish");
    let held_identity = mailbox::identity(&held);
    assert!(matches!(
        gate.admit_mailbox(Some(ReceiverClass::Prompting), &held),
        MailboxAdmission::Held { .. }
    ));
    let id = gate.held_messages()[0].id.clone();
    let admitted_identity = mailbox::identity(&message("admitted"));
    gate.admit_identity(admitted_identity.clone());

    // Both still present: nothing moves.
    let present: HashSet<_> = [held_identity.clone(), admitted_identity.clone()]
        .into_iter()
        .collect();
    gate.reconcile(&present);
    assert_eq!(gate.held_messages().len(), 1);
    assert_eq!(
        gate.disposition(&admitted_identity),
        PassDisposition::Deliver
    );

    gate.reconcile(&HashSet::new());
    assert!(gate.held_messages().is_empty());
    assert_eq!(gate.disposition(&held_identity), PassDisposition::Classify);
    assert_eq!(
        gate.disposition(&admitted_identity),
        PassDisposition::Classify
    );
    let last = drained(&mut drain).pop();
    assert_eq!(
        last,
        Some(HoldTransition::Settled {
            id,
            outcome: HeldOutcome::Expired
        })
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_settles_everything_expired_and_is_idempotent() {
    let (gate, mut drain) = Inbound::new(explicit(InboundPolicy::Hold), DialogExpiry::default());
    assert!(matches!(
        gate.admit_socket(Some(ReceiverClass::Prompting), "w@t", "one", None),
        SocketAdmission::Held { .. }
    ));
    assert!(matches!(
        gate.admit_mailbox(Some(ReceiverClass::Prompting), &message("two")),
        MailboxAdmission::Held { .. }
    ));
    gate.shutdown_settle();
    assert!(gate.held_messages().is_empty());
    let expired = drained(&mut drain)
        .into_iter()
        .filter(|transition| {
            matches!(
                transition,
                HoldTransition::Settled {
                    outcome: HeldOutcome::Expired,
                    ..
                }
            )
        })
        .count();
    assert_eq!(expired, 2);

    gate.shutdown_settle();
    assert!(drained(&mut drain).is_empty());
}

// Guard drops answer the caller as typed reasons; the socket door's
// Silent variant carries them for tracing, never for the response body.
#[tokio::test(start_paused = true)]
async fn a_socket_guard_drop_is_silent_with_its_typed_reason() {
    let (gate, _drain) = Inbound::new(explicit(InboundPolicy::Accept), DialogExpiry::default());
    assert_eq!(
        gate.admit_socket(Some(ReceiverClass::Prompting), "w@t", "same", None),
        SocketAdmission::Deliver
    );
    assert_eq!(
        gate.admit_socket(Some(ReceiverClass::Prompting), "w@t", "same", None),
        SocketAdmission::Silent(DropReason::Guard(Dropped::Duplicate))
    );
    let (unset_gate, _drain) = Inbound::new(unset(), DialogExpiry::default());
    assert_eq!(
        unset_gate.admit_socket(Some(ReceiverClass::Prompting), "w@t", "hello", None),
        SocketAdmission::Deliver
    );
}

#[tokio::test(start_paused = true)]
async fn an_explicit_refuse_drops_on_both_doors_naming_its_tier() {
    let (gate, _drain) = Inbound::new(
        ResolvedInbound::new(Some((InboundPolicy::Refuse, PolicySource::Project))),
        DialogExpiry::default(),
    );
    let refused = RefuseCause::Explicit {
        source: PolicySource::Project,
    };
    assert_eq!(
        gate.admit_socket(Some(ReceiverClass::Prompting), "w@t", "no", None),
        SocketAdmission::Silent(DropReason::Refused(refused))
    );
    assert_eq!(
        gate.admit_mailbox(Some(ReceiverClass::Prompting), &message("no")),
        MailboxAdmission::Drop(DropReason::Refused(refused))
    );
    assert!(gate.held_messages().is_empty());
}

// The transition stamps into the protocol event whole, and its
// body-bearing fields debug as sizes (M4).
#[test]
fn a_transition_stamps_into_its_event_and_debugs_no_bodies() {
    let id = HeldId::ascending();
    let transition = HoldTransition::Held {
        id: id.clone(),
        from: "w1@t".to_owned(),
        cause: HoldCause::NoModeAsserted,
        summary: None,
        preview: RedactedText::from("a secret body".to_owned()),
        expires_in_ms: Some(1000),
    };
    let debugged = format!("{transition:?}");
    assert!(!debugged.contains("a secret body"), "{debugged}");
    assert!(debugged.contains("<13 bytes>"), "{debugged}");

    let session = SessionId::from("s1".to_owned());
    match transition.into_event(session.clone()) {
        Event::PeerHeld {
            session_id,
            id: event_id,
            cause,
            ..
        } => {
            assert_eq!(session_id, session);
            assert_eq!(event_id, id);
            assert_eq!(cause, HoldCause::NoModeAsserted);
        }
        other => panic!("a hold stamps into PeerHeld: {other:?}"),
    }
    match (HoldTransition::Settled {
        id: id.clone(),
        outcome: HeldOutcome::Expired,
    })
    .into_event(session.clone())
    {
        Event::PeerHoldSettled {
            session_id,
            id: event_id,
            outcome,
        } => {
            assert_eq!(session_id, session);
            assert_eq!(event_id, id);
            assert_eq!(outcome, HeldOutcome::Expired);
        }
        other => panic!("a settle stamps into PeerHoldSettled: {other:?}"),
    }
}

#[test]
fn a_released_message_debugs_sizes_never_text() {
    let released = ReleasedMessage {
        from: "w1@t".to_owned(),
        text: "the body".to_owned(),
        summary: Some("the summary".to_owned()),
    };
    let debugged = format!("{released:?}");
    assert!(!debugged.contains("the body"), "{debugged}");
    assert!(!debugged.contains("the summary"), "{debugged}");
    assert!(debugged.contains("w1@t"), "{debugged}");
}

#[test]
fn the_preview_caps_lines_and_chars_and_strips_control_characters() {
    let many_lines: String = (0..20).map(|i| format!("line {i}\n")).collect();
    let preview = preview_of(&many_lines);
    assert_eq!(preview.lines().count(), PREVIEW_LINES);

    let wide = "x".repeat(PREVIEW_CHARS + 100);
    assert_eq!(preview_of(&wide).chars().count(), PREVIEW_CHARS);

    // Neutralized, not erased: the ESC and BEL bytes go, so the sequence
    // cannot execute, and its printable remainder stays visible.
    assert_eq!(preview_of("a\u{1b}[31mb\tc\nd\u{7}"), "a[31mb\tc\nd");
}

// A settle naming an id nobody holds is ignored without error — the
// reply-races-cancel rule.
#[tokio::test(start_paused = true)]
async fn a_settle_for_an_unknown_id_is_a_silent_no_op() {
    let (gate, mut drain) = Inbound::new(unset(), DialogExpiry::default());
    let unknown = HeldId::ascending();
    assert_eq!(gate.release(&unknown), None);
    assert_eq!(gate.deny(&unknown), None);
    assert_eq!(gate.expire(&unknown), None);
    assert_eq!(gate.pruned(&unknown), None);
    gate.prune_failed(&unknown);
    assert!(drained(&mut drain).is_empty());
}
