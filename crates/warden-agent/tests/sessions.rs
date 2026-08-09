use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};
use uuid::Uuid;
use warden_agent::{
    AgentError, AgentInput, AgentRequest, AgentResponse, AgentSessions, Conversation,
    InvocationEnvironment, ProviderDriver, ProviderKind, ResumeMetadata, SessionKey,
    SessionSnapshot,
};

#[derive(Clone, Debug)]
struct RecordedCall {
    sequence: u64,
    conversation: Conversation,
    model: Option<String>,
}

#[derive(Default)]
struct FakeDriver {
    calls: Mutex<Vec<RecordedCall>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl FakeDriver {
    fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().expect("calls mutex").clone()
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ProviderDriver for FakeDriver {
    fn provider(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    async fn invoke(&self, request: AgentRequest) -> Result<AgentResponse, AgentError> {
        self.calls.lock().expect("calls mutex").push(RecordedCall {
            sequence: request.input.source_sequence,
            conversation: request.conversation.clone(),
            model: request.model.clone(),
        });
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        let delay = request
            .input
            .event
            .get("delay_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        sleep(Duration::from_millis(delay)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);

        if request.input.event.get("fail").and_then(Value::as_bool) == Some(true) {
            return Err(AgentError::ProviderFailure {
                provider: ProviderKind::Claude,
                message: "simulated provider crash".to_owned(),
            });
        }

        let resume = match &request.conversation {
            Conversation::Fresh => None,
            Conversation::Persistent { resume } => Some(
                resume
                    .clone()
                    .unwrap_or_else(|| ResumeMetadata::new(ProviderKind::Claude, "session-1")),
            ),
        };
        Ok(AgentResponse {
            provider: ProviderKind::Claude,
            invocation_id: request.invocation_id,
            source_sequence: request.input.source_sequence,
            text: Some(format!("sequence {}", request.input.source_sequence)),
            structured_output: None,
            events: vec![json!({"type":"result"})],
            usage: None,
            resume,
        })
    }

    async fn interrupt(&self, _invocation_id: Uuid) -> Result<bool, AgentError> {
        Ok(false)
    }

    async fn shutdown(&self) -> Result<(), AgentError> {
        Ok(())
    }
}

fn runtime(driver: &Arc<FakeDriver>) -> Arc<AgentSessions> {
    let registered: Arc<dyn ProviderDriver> = driver.clone();
    Arc::new(AgentSessions::new([registered]).expect("agent sessions"))
}

fn key(name: &str) -> SessionKey {
    SessionKey::new(ProviderKind::Claude, "hook", name, "source-thread")
}

#[tokio::test]
async fn fresh_calls_never_receive_resume_context() {
    let driver = Arc::new(FakeDriver::default());
    let sessions = runtime(&driver);

    sessions
        .run_fresh(ProviderKind::Claude, AgentInput::new(1, json!({"event":1})))
        .await
        .expect("fresh call one");
    sessions
        .run_fresh(ProviderKind::Claude, AgentInput::new(2, json!({"event":2})))
        .await
        .expect("fresh call two");

    let calls = driver.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|call| call.conversation == Conversation::Fresh)
    );
}

#[tokio::test]
async fn one_persistent_session_serializes_events_in_source_order() {
    let driver = Arc::new(FakeDriver::default());
    let sessions = runtime(&driver);
    let session_key = key("ordered");

    let first_sessions = Arc::clone(&sessions);
    let first_key = session_key.clone();
    let first = tokio::spawn(async move {
        first_sessions
            .send_persistent(
                first_key,
                AgentInput::new(10, json!({"delay_ms":80,"event":10})),
            )
            .await
    });
    tokio::task::yield_now().await;
    let second_sessions = Arc::clone(&sessions);
    let second_key = session_key.clone();
    let second = tokio::spawn(async move {
        second_sessions
            .send_persistent(second_key, AgentInput::new(11, json!({"event":11})))
            .await
    });

    first.await.expect("first join").expect("first send");
    second.await.expect("second join").expect("second send");

    let calls = driver.calls();
    assert_eq!(
        calls.iter().map(|call| call.sequence).collect::<Vec<_>>(),
        [10, 11]
    );
    assert_eq!(driver.max_active(), 1);
    assert!(matches!(
        calls[0].conversation,
        Conversation::Persistent { resume: None }
    ));
    assert!(matches!(
        calls[1].conversation,
        Conversation::Persistent { resume: Some(_) }
    ));

    let snapshot = sessions
        .session_snapshot(&session_key)
        .await
        .expect("snapshot");
    assert_eq!(snapshot.resume.session_id, "session-1");
    assert_eq!(snapshot.last_successful_source_sequence, Some(11));
}

#[tokio::test]
async fn durable_metadata_can_be_restored_after_runtime_restart() {
    let first_epoch = Uuid::new_v4();
    let first_driver = Arc::new(FakeDriver::default());
    let first_runtime = runtime(&first_driver);
    let session_key = key("restored");
    first_runtime
        .send_persistent(
            session_key.clone(),
            AgentInput::new(20, json!({"event":20})).with_source_epoch(first_epoch),
        )
        .await
        .expect("first send");
    let snapshot = first_runtime
        .session_snapshot(&session_key)
        .await
        .expect("snapshot");

    let resumed_driver = Arc::new(FakeDriver::default());
    let resumed_runtime = runtime(&resumed_driver);
    resumed_runtime
        .restore_session(session_key.clone(), snapshot)
        .await
        .expect("restore");
    resumed_runtime
        .send_persistent(
            session_key,
            AgentInput::new(1, json!({"event":1})).with_source_epoch(Uuid::new_v4()),
        )
        .await
        .expect("new ingress epoch accepts its own sequence space");

    let calls = resumed_driver.calls();
    assert!(matches!(
        &calls[0].conversation,
        Conversation::Persistent { resume: Some(metadata) }
            if metadata.session_id == "session-1"
    ));
}

#[tokio::test]
async fn persistent_session_binds_and_restores_its_model() {
    let driver = Arc::new(FakeDriver::default());
    let sessions = runtime(&driver);
    let session_key = key("model-bound");

    sessions
        .send_persistent_with_options(
            session_key.clone(),
            AgentInput::new(1, json!({"event":1})),
            Some("sonnet".into()),
            InvocationEnvironment::default(),
        )
        .await
        .expect("start with Sonnet");
    sessions
        .send_persistent_with_options(
            session_key.clone(),
            AgentInput::new(2, json!({"event":2})),
            Some("sonnet".into()),
            InvocationEnvironment::default(),
        )
        .await
        .expect("resume with Sonnet");

    let mismatch = sessions
        .send_persistent_with_options(
            session_key.clone(),
            AgentInput::new(3, json!({"event":3})),
            Some("opus".into()),
            InvocationEnvironment::default(),
        )
        .await
        .expect_err("model switch must fail");
    assert!(matches!(
        mismatch,
        AgentError::SessionModelMismatch {
            requested: Some(requested),
            bound: Some(bound),
        } if requested == "opus" && bound == "sonnet"
    ));
    assert_eq!(driver.calls().len(), 2);
    assert!(
        driver
            .calls()
            .iter()
            .all(|call| call.model.as_deref() == Some("sonnet"))
    );

    let snapshot = sessions.session_snapshot(&session_key).await.unwrap();
    assert_eq!(snapshot.model.as_deref(), Some("sonnet"));
    let resumed_driver = Arc::new(FakeDriver::default());
    let resumed = runtime(&resumed_driver);
    resumed
        .restore_session(session_key.clone(), snapshot)
        .await
        .expect("restore model-bound session");
    resumed
        .send_persistent_with_options(
            session_key,
            AgentInput::new(1, json!({"event":"new epoch"})).with_source_epoch(Uuid::new_v4()),
            Some("sonnet".into()),
            InvocationEnvironment::default(),
        )
        .await
        .expect("restored session keeps Sonnet");
}

#[tokio::test]
async fn legacy_model_less_snapshot_remains_bound_to_provider_default() {
    let driver = Arc::new(FakeDriver::default());
    let sessions = runtime(&driver);
    let session_key = key("legacy-default");
    sessions
        .restore_session(
            session_key.clone(),
            SessionSnapshot {
                resume: ResumeMetadata::new(ProviderKind::Claude, "legacy-session"),
                model: None,
                last_successful_source_epoch: None,
                last_successful_source_sequence: Some(1),
                last_successful_source_ordinal: Some(0),
            },
        )
        .await
        .expect("restore legacy snapshot");

    let mismatch = sessions
        .send_persistent_with_options(
            session_key.clone(),
            AgentInput::new(2, json!({"event":2})),
            Some("sonnet".into()),
            InvocationEnvironment::default(),
        )
        .await
        .expect_err("legacy default session must not silently switch");
    assert!(matches!(
        mismatch,
        AgentError::SessionModelMismatch {
            requested: Some(_),
            bound: None,
        }
    ));

    sessions
        .send_persistent(session_key, AgentInput::new(2, json!({"event":2})))
        .await
        .expect("provider-default resume remains valid");
    assert_eq!(driver.calls()[0].model, None);
}

#[tokio::test]
async fn failed_session_does_not_block_an_unrelated_session() {
    let driver = Arc::new(FakeDriver::default());
    let sessions = runtime(&driver);

    let bad_runtime = Arc::clone(&sessions);
    let bad = tokio::spawn(async move {
        bad_runtime
            .send_persistent(
                key("bad"),
                AgentInput::new(1, json!({"delay_ms":180,"fail":true})),
            )
            .await
    });
    tokio::task::yield_now().await;

    let started = Instant::now();
    sessions
        .send_persistent(
            key("good"),
            AgentInput::new(1, json!({"delay_ms":5,"event":"good"})),
        )
        .await
        .expect("unrelated session succeeds");
    assert!(started.elapsed() < Duration::from_millis(100));

    let error = bad
        .await
        .expect("bad join")
        .expect_err("bad provider fails");
    assert!(matches!(error, AgentError::ProviderFailure { .. }));
    assert!(driver.max_active() >= 2);
}

#[tokio::test]
async fn dormant_sessions_do_not_receive_events_and_sequences_cannot_regress() {
    let driver = Arc::new(FakeDriver::default());
    let sessions = runtime(&driver);
    let session_key = key("dormant");
    sessions
        .send_persistent(
            session_key.clone(),
            AgentInput::new(30, json!({"active_turn":true})),
        )
        .await
        .expect("active event");

    // Persistent sessions have no autonomous event subscription. An inactive
    // turn therefore produces no provider call unless the activation router
    // explicitly invokes `send_persistent`.
    tokio::task::yield_now().await;
    assert_eq!(driver.calls().len(), 1);

    let error = sessions
        .send_persistent(session_key, AgentInput::new(30, json!({"duplicate":true})))
        .await
        .expect_err("duplicate sequence rejected");
    assert!(matches!(
        error,
        AgentError::SequenceRegression {
            incoming: 30,
            last: 30
        }
    ));
    assert_eq!(driver.calls().len(), 1);
}

#[tokio::test]
async fn normalized_events_from_one_source_sequence_use_their_ordinal() {
    let driver = Arc::new(FakeDriver::default());
    let sessions = runtime(&driver);
    let session_key = key("same-source");
    sessions
        .send_persistent(
            session_key.clone(),
            AgentInput::new(40, json!({"kind":"user_prompt_submitted"})).with_source_ordinal(0),
        )
        .await
        .expect("first normalized event");
    sessions
        .send_persistent(
            session_key.clone(),
            AgentInput::new(40, json!({"kind":"turn_started"})).with_source_ordinal(1),
        )
        .await
        .expect("second normalized event from the same frame");
    let duplicate = sessions
        .send_persistent(
            session_key,
            AgentInput::new(40, json!({"kind":"turn_started"})).with_source_ordinal(1),
        )
        .await;
    assert!(matches!(
        duplicate,
        Err(AgentError::SequenceRegression { .. })
    ));
    assert_eq!(driver.calls().len(), 2);
}

#[tokio::test]
async fn durable_commit_precedes_memory_publication_and_failure_quarantines_the_key() {
    let driver = Arc::new(FakeDriver::default());
    let sessions = runtime(&driver);
    let session_key = key("commit-failure");
    let prepared = Arc::new(AtomicUsize::new(0));
    let committed = Arc::new(AtomicUsize::new(0));

    let error = sessions
        .send_persistent_transactional(
            session_key.clone(),
            AgentInput::new(50, json!({"event":50})),
            Default::default(),
            {
                let prepared = prepared.clone();
                move || async move {
                    prepared.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
            {
                let committed = committed.clone();
                move |snapshot| async move {
                    assert_eq!(snapshot.last_successful_source_sequence, Some(50));
                    committed.fetch_add(1, Ordering::SeqCst);
                    Err("simulated durable publication failure".into())
                }
            },
        )
        .await
        .expect_err("durable publication failure must fail the send");

    assert!(matches!(error, AgentError::DurableCommitFailed { .. }));
    assert_eq!(prepared.load(Ordering::SeqCst), 1);
    assert_eq!(committed.load(Ordering::SeqCst), 1);
    assert!(sessions.session_snapshot(&session_key).await.is_none());
    assert!(matches!(
        sessions.session_status(&session_key).await,
        Err(AgentError::SessionUnavailable { .. })
    ));
    assert!(matches!(
        sessions
            .send_persistent(
                session_key.clone(),
                AgentInput::new(51, json!({"event":51}))
            )
            .await,
        Err(AgentError::SessionUnavailable { .. })
    ));
    assert_eq!(driver.calls().len(), 1);

    assert!(sessions.reset_session(&session_key).await);
    sessions
        .send_persistent(session_key, AgentInput::new(51, json!({"event":51})))
        .await
        .expect("explicit reset restores availability");
    assert_eq!(driver.calls().len(), 2);
}
