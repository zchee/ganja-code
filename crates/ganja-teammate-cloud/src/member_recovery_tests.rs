use futures::StreamExt as _;
use ganja_core::provider::ProviderEvent;
use ganja_core::{Engine, Storage};
use ganja_protocol::Command;
use ganja_serve::{EventJournal, Handle, Listen, ServeConfig};

use super::*;

#[derive(Debug)]
struct GapSource {
    inner: ClientSource,
    gap: AtomicBool,
    gap_after_finish: AtomicBool,
    prefix: Mutex<VecDeque<Resumed>>,
}

#[async_trait]
impl EventSource for GapSource {
    async fn connect(&self) -> Result<Option<Connected>, ClientError> {
        self.inner.connect().await
    }

    async fn next(&self) -> Option<Result<Resumed, ClientError>> {
        let prefix = self.prefix.lock().expect("prefix events").pop_front();
        if let Some(prefix) = prefix {
            return Some(Ok(prefix));
        }
        if self.gap_after_finish.swap(false, Ordering::SeqCst) {
            loop {
                let item = self.inner.next().await?;
                if matches!(&item, Ok(Resumed::Event(event)) if matches!(**event, Event::MessageFinished { .. }))
                {
                    self.gap.store(true, Ordering::SeqCst);
                    break;
                }
                if item.is_err() {
                    return Some(item);
                }
            }
        }
        if self.gap.swap(false, Ordering::SeqCst) {
            self.inner.invalidate().await;
            self.inner.release();
            return match self.inner.connect().await {
                Ok(Some(connected)) => Some(Ok(Resumed::Gap { head: connected.head })),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            };
        }
        self.inner.next().await
    }

    async fn hold(&self) {
        self.inner.hold().await;
    }

    fn release(&self) {
        self.inner.release();
    }

    async fn invalidate(&self) {
        self.inner.invalidate().await;
    }

    async fn cursor(&self) -> Option<EventId> {
        self.inner.cursor().await
    }
}

struct Running {
    fixture: Fixture,
    engine: Arc<Engine>,
    storage: Storage,
    journal: EventJournal,
    server: Handle,
    source: Arc<GapSource>,
}

impl Running {
    async fn new(script: Vec<Vec<ProviderEvent>>) -> Self {
        Self::with_blocker(script, false).await.0
    }

    async fn with_blocker(
        script: Vec<Vec<ProviderEvent>>,
        block: bool,
    ) -> (Self, Option<futures::stream::BoxStream<'static, Event>>) {
        let mut fixture = Fixture::new().await;
        let storage = Storage::open(fixture._home.path().join("storage"));
        let session = ganja_testkit::seed_session(&storage, 0);
        let (provider, _) = ganja_testkit::ScriptedProvider::named("fake", script);
        let engine = Arc::new(Engine::persistent(
            provider,
            "fake",
            Arc::new(ganja_core::tool::Registry::new(Vec::new())),
            ganja_core::permission::Permissions::default(),
            storage.clone(),
        ));
        engine.resume(&session).await.expect("resume seeded session");
        let blocker =
            if block { Some(engine.subscribe().await.expect("blocking subscriber")) } else { None };
        let mut config = ServeConfig::in_directory(fixture._home.path().to_owned());
        config.listen = Listen::loopback();
        config.storage = Some(storage.clone());
        let journal = config.journal.clone();
        let server = ganja_serve::serve(engine.clone(), config).await.expect("real serve");
        let client = Client::new(&format!("http://{}", server.address()), None).expect("client");
        let source = Arc::new(GapSource {
            inner: ClientSource::new(client.clone(), fixture.member.gate.clone()),
            gap: AtomicBool::new(false),
            gap_after_finish: AtomicBool::new(false),
            prefix: Mutex::new(VecDeque::new()),
        });
        let member = Arc::get_mut(&mut fixture.member).expect("unique fixture member");
        member.client = Some(client);
        member.events = Some(source.clone());
        Arc::get_mut(&mut member.runtime).expect("unique runtime").deadlines.terminal =
            Duration::from_secs(1);
        *member.session.lock().expect("session") = Some(session);
        let connected = source.connect().await.expect("connect").expect("open source");
        member.connected(&connected);
        (Self { fixture, engine, storage, journal, server, source }, blocker)
    }

    async fn restart(mut self) -> Self {
        self.source.hold().await;
        self.server.shutdown().await.expect("stop old incarnation");
        let mut config = ServeConfig::in_directory(self.fixture._home.path().to_owned());
        config.listen = Listen::loopback();
        config.storage = Some(self.storage.clone());
        self.journal = config.journal.clone();
        self.server =
            ganja_serve::serve(self.engine.clone(), config).await.expect("new incarnation");
        let client =
            Client::new(&format!("http://{}", self.server.address()), None).expect("client");
        self.source = Arc::new(GapSource {
            inner: ClientSource::new(client.clone(), self.fixture.member.gate.clone()),
            gap: AtomicBool::new(true),
            gap_after_finish: AtomicBool::new(false),
            prefix: Mutex::new(VecDeque::new()),
        });
        let member = Arc::get_mut(&mut self.fixture.member).expect("unique member");
        member.client = Some(client);
        member.events = Some(self.source.clone());
        self
    }

    fn mail(&self) -> Vec<MailboxMessage> {
        mailbox::read(&self.fixture.member.lent.lead_inbox).expect("mailbox").valid
    }

    async fn close(self) {
        self.source.hold().await;
        self.server.shutdown().await.expect("stop real serve");
    }
}

fn prompt() -> Command {
    Command::SendPrompt {
        text: "answer".to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
        peers: Vec::new(),
        session_mentions: Vec::new(),
    }
}

fn long_answer() -> Vec<ProviderEvent> {
    let mut answer =
        vec![ProviderEvent::TextDelta("x".to_owned()); ganja_core::engine::EVENT_CAPACITY + 32];
    answer.push(ProviderEvent::Finish(FinishReason::Completed));
    answer
}

#[tokio::test]
async fn a_saved_envelope_is_mailed_once_when_its_finish_arrives_after_the_gap() {
    let reference = Running::new(vec![long_answer()]).await;
    let mut observed = reference.engine.subscribe().await.expect("reference events");
    reference.engine.send(prompt()).await.expect("reference prompt");
    let expected = ganja_testkit::drain(&mut observed).await;
    assert!(matches!(expected.last(), Some(Event::MessageFinished { .. })));
    let pre_finish = expected.len() - 1;
    reference.close().await;

    let (running, blocked) = Running::with_blocker(vec![long_answer()], true).await;
    let mut blocked = blocked.expect("first subscriber blocks publication");
    let mut direct = running.engine.subscribe().await.expect("measure events");
    running.engine.send(prompt()).await.expect("prompt");
    let draining = tokio::spawn(async move { ganja_testkit::drain(&mut direct).await });
    // Free only the measured pre-finish slots; the finish stays backpressured.
    for _ in 0..pre_finish - ganja_core::engine::EVENT_CAPACITY {
        blocked.next().await.expect("pre-finish event");
    }
    let completed = timeout(Duration::from_secs(3), async {
        loop {
            if let Some(message) = running
                .storage
                .load_transcript(&running.engine.session_id())
                .expect("transcript")
                .into_iter()
                .find(|message| message.role == Role::Assistant && message.time.completed.is_some())
            {
                break message;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("saved envelope before finish publication");
    running.source.gap.store(true, Ordering::SeqCst);
    running.fixture.member.wait_for_terminal(None, None, false).await.expect("gap convergence");
    let before = running.mail();
    assert_eq!(before.len(), 2, "one final text and one idle notification");
    assert!(before.iter().any(|mail| mail.text.contains("final text may be incomplete")));
    assert!(before.iter().any(|mail| matches!(serde_json::from_str::<Frame>(&mail.text), Ok(Frame::IdleNotification(ref idle)) if idle.idle_reason == Some(IdleReason::Interrupted))));
    assert_eq!(
        running.fixture.member.terminal.lock().expect("decisions").get(&completed.id),
        Some(&None)
    );
    drop(blocked);
    draining.await.expect("drain joined");
    let result = running.fixture.member.wait_for_terminal(None, None, true).await;
    assert_eq!(result, Err("terminal state unavailable"));
    assert_eq!(
        running.fixture.member.terminal.lock().expect("decisions").get(&completed.id),
        Some(&Some(FinishReason::Completed))
    );
    assert_eq!(running.mail().len(), before.len(), "retained finish cannot mail twice");
    running.close().await;
}

#[tokio::test]
async fn only_shorter_recovered_text_gets_a_positive_loss_marker() {
    for (prefix, lost) in [("a much longer streamed prefix", true), ("stored", false), ("s", false)]
    {
        let running = Running::new(vec![ganja_testkit::says("stored")]).await;
        let mut events = running.engine.subscribe().await.expect("events");
        running.engine.send(prompt()).await.expect("prompt");
        let events = ganja_testkit::drain(&mut events).await;
        let mut started = events.into_iter().find(|event| matches!(event, Event::MessageStarted { message, .. } if message.role == Role::Assistant)).expect("assistant start");
        let transcript = running
            .storage
            .load_transcript(&running.engine.session_id())
            .expect("stored transcript");
        if let Event::MessageStarted { message, .. } = &mut started {
            *message = transcript
                .into_iter()
                .find(|stored| stored.id == message.id)
                .expect("stored assistant");
            message.time.completed = None;
            for part in &mut message.parts {
                if let PartBody::Text { text } = &mut part.body {
                    *text = prefix.to_owned();
                }
            }
        }
        running
            .source
            .prefix
            .lock()
            .expect("prefix events")
            .push_back(Resumed::Event(Box::new(started)));
        running.source.gap.store(true, Ordering::SeqCst);
        running.fixture.member.wait_for_terminal(None, None, false).await.expect("gap convergence");
        let mail = running.mail();
        let text = &mail
            .iter()
            .find(|message| message.text.contains("final text may be incomplete"))
            .expect("gap mail")
            .text;
        assert!(text.starts_with("stored"), "mail uses storage, never a stitched prefix");
        assert_eq!(text.contains("text lost"), lost);
        assert!(!text.contains("much longer"), "missing text is never synthesized");
        assert_eq!(mail.len(), 2);
        running.close().await;
    }
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn text_loss_evidence_survives_a_terminal_reconciliation_timeout() {
    let running = Running::new(vec![ganja_testkit::says("stored")]).await;
    let fault = ganja_core::session::CompletedEnvelopeFailure::new(running.engine.session_id());
    let mut events = running.engine.subscribe().await.expect("events");
    running.engine.send(prompt()).await.expect("prompt");
    let events = ganja_testkit::drain(&mut events).await;
    let mut started = events.into_iter().find(|event| matches!(event, Event::MessageStarted { message, .. } if message.role == Role::Assistant)).expect("assistant start");
    let transcript =
        running.storage.load_transcript(&running.engine.session_id()).expect("transcript");
    if let Event::MessageStarted { message, .. } = &mut started {
        *message = transcript
            .into_iter()
            .find(|stored| stored.id == message.id)
            .expect("stored assistant");
        for part in &mut message.parts {
            if let PartBody::Text { text } = &mut part.body {
                *text = "a much longer streamed prefix".to_owned();
            }
        }
    }
    running
        .source
        .prefix
        .lock()
        .expect("prefix events")
        .push_back(Resumed::Event(Box::new(started)));
    running.source.gap.store(true, Ordering::SeqCst);
    assert_eq!(
        running.fixture.member.wait_for_terminal(None, None, false).await,
        Err("terminal state unavailable")
    );
    assert!(!running.fixture.member.observed_text.lock().expect("byte counts").is_empty());
    running.fixture.member.set_phase(Phase::RecoveryRequired);
    CloudControl(running.fixture.member.clone())
        .resume_for_inspection()
        .await
        .expect("explicit continuation");
    let mail = running.mail();
    assert_eq!(mail.len(), 2);
    let text = &mail
        .iter()
        .find(|message| message.text.contains("final text may be incomplete"))
        .expect("gap mail")
        .text;
    assert!(text.starts_with("stored"));
    assert!(text.contains("text lost"));
    assert!(!text.contains("longer streamed"));
    drop(fault);
    running.close().await;
}

#[tokio::test]
async fn a_new_epoch_converges_to_the_same_idle_transcript() {
    let running = Running::new(vec![ganja_testkit::says("persisted answer")]).await;
    let mut events = running.engine.subscribe().await.expect("events");
    running.engine.send(prompt()).await.expect("prompt");
    ganja_testkit::drain(&mut events).await;
    let before = running.storage.load_transcript(&running.engine.session_id()).expect("before");
    let epoch = running.journal.epoch().to_owned();
    let running = running.restart().await;
    assert_ne!(running.journal.epoch(), epoch);
    running.fixture.member.wait_for_terminal(None, None, false).await.expect("cold convergence");
    let after = running.storage.load_transcript(&running.engine.session_id()).expect("after");
    assert_eq!(before, after);
    assert_eq!(running.mail().len(), 2);
    assert!(
        running.mail().iter().any(|message| message.text.contains("final text may be incomplete"))
    );
    running.close().await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn a_new_epoch_interrupts_a_stored_turn_without_a_completed_envelope() {
    let running = Running::new(vec![ganja_testkit::says("interrupted answer")]).await;
    let fault = ganja_core::session::CompletedEnvelopeFailure::new(running.engine.session_id());
    let mut events = running.engine.subscribe().await.expect("events");
    running.engine.send(prompt()).await.expect("prompt");
    ganja_testkit::drain(&mut events).await;
    let running = running.restart().await;
    running
        .fixture
        .member
        .wait_for_terminal(None, None, false)
        .await
        .expect("cold interrupted turn");
    assert_eq!(running.mail().len(), 2);
    assert!(running.mail().iter().any(|message| matches!(serde_json::from_str::<Frame>(&message.text), Ok(Frame::IdleNotification(ref idle)) if idle.idle_reason == Some(IdleReason::Interrupted))));
    assert_eq!(running.fixture.member.terminal.lock().expect("decisions").len(), 1);
    drop(fault);
    running.close().await;
}

#[tokio::test]
async fn an_owner_reported_revert_invalidates_before_any_explicit_resume() {
    let running = Running::new(vec![ganja_testkit::says("old answer")]).await;
    running.fixture.member.turn("team-lead", "first request").await.expect("first turn");
    assert_eq!(running.mail().len(), 2);
    assert!(!running.fixture.member.terminal.lock().expect("decisions").is_empty());
    let control = CloudControl(running.fixture.member.clone());
    running.fixture.provisioner.owner_recover(&running.fixture.worker);
    control.report_external_revert().await;
    assert_eq!(control.phase(), Phase::Suspended);
    assert!(running.source.cursor().await.is_none(), "held checkpoint cursor was invalidated");
    assert!(running.fixture.member.terminal.lock().expect("decisions").is_empty());
    assert_eq!(running.mail().len(), 2, "reporting a revert must not resume or mail automatically");
    assert!(!running.fixture.member.gate.state().stream_open);
    assert!(!running.fixture.member.gate.state().rest_open);
    control.resume_for_inspection().await.expect("explicit resume");
    assert_eq!(running.mail().len(), 4, "the new history gets its own conservative decision");
    assert_eq!(control.phase(), Phase::Idle);
    running.close().await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn a_failed_envelope_with_a_pre_gap_finish_requires_bounded_recovery() {
    let running = Running::new(vec![ganja_testkit::says("complete text")]).await;
    let fault = ganja_core::session::CompletedEnvelopeFailure::new(running.engine.session_id());
    let mut direct = running.engine.subscribe().await.expect("events");
    running.engine.send(prompt()).await.expect("prompt");
    let events = ganja_testkit::drain(&mut direct).await;
    let id = events
        .iter()
        .find_map(|event| match event {
            Event::MessageFinished { message_id, .. } => Some(message_id.clone()),
            _ => None,
        })
        .expect("finish");
    timeout(Duration::from_secs(1), async {
        while running
            .journal
            .head()
            .rsplit_once('-')
            .expect("head")
            .1
            .parse::<usize>()
            .expect("sequence")
            < events.len()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("journal consumed finish");
    let stored = running.storage.load_transcript(&running.engine.session_id()).expect("store");
    assert!(
        stored.iter().find(|message| message.id == id).expect("assistant").time.completed.is_none()
    );
    running.source.gap.store(true, Ordering::SeqCst);
    assert_eq!(
        running.fixture.member.wait_for_terminal(None, None, false).await,
        Err("terminal state unavailable")
    );
    assert!(running.mail().is_empty(), "no success or idle claim from an incomplete envelope");
    assert!(running.fixture.member.terminal.lock().expect("decisions").is_empty());
    drop(fault);
    running.close().await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn a_missing_finish_mails_once_and_only_an_explicit_message_continues() {
    let running = Running::new(vec![
        ganja_testkit::says("first answer"),
        ganja_testkit::says("second answer"),
    ])
    .await;
    let fault = ganja_core::session::CompletedEnvelopeFailure::new(running.engine.session_id());
    running.source.gap_after_finish.store(true, Ordering::SeqCst);
    let member = running.fixture.member.clone();
    let task = tokio::spawn(member.clone().run());
    let send = |text: &str| {
        mailbox::write(
            &member.spec.inbox(),
            MailboxMessage::new("team-lead", text, record::now_iso8601()),
        )
        .expect("inbox write");
    };
    send("first request");
    timeout(Duration::from_secs(4), async {
        loop {
            if member.phase() == Phase::RecoveryRequired && running.mail().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bounded recovery notice");
    assert!(running.mail()[0].text.contains("terminal state unavailable"));
    assert_eq!(
        running
            .storage
            .load_transcript(&running.engine.session_id())
            .expect("transcript")
            .iter()
            .filter(|message| message.role == Role::User)
            .count(),
        1,
        "the uncertain prompt is never resent"
    );
    assert_eq!(running.fixture.suspends(), 0, "recovery cannot claim an idle boundary");
    drop(fault);
    send("continue explicitly");
    timeout(Duration::from_secs(4), async {
        loop {
            if member.phase() == Phase::Suspended && running.mail().len() == 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owner continuation completes both decisions");
    let mail = running.mail();
    assert_eq!(
        mail.iter().filter(|message| message.text.contains("terminal state unavailable")).count(),
        1
    );
    assert_eq!(
        mail.iter().filter(|message| message.text.contains("final text may be incomplete")).count(),
        1
    );
    assert_eq!(
        running
            .storage
            .load_transcript(&running.engine.session_id())
            .expect("transcript")
            .iter()
            .filter(|message| message.role == Role::User)
            .count(),
        2
    );
    member.lent.cancel.cancel();
    task.await.expect("member loop joined");
    running.close().await;
}
