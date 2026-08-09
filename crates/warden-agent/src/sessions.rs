use std::{collections::HashMap, future::Future, sync::Arc};

use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    AgentError, AgentInput, AgentRequest, AgentResponse, InvocationEnvironment, ProviderDriver,
    ProviderKind, ResumeMetadata, SessionKey, SessionSnapshot,
};

#[derive(Default)]
struct SessionState {
    resume: Option<ResumeMetadata>,
    last_successful_source_epoch: Option<Uuid>,
    last_successful_source_sequence: Option<u64>,
    last_successful_source_ordinal: Option<u16>,
    unavailable: Option<String>,
}

#[derive(Default)]
struct SessionSlot {
    /// Holding this lock across provider invocation deliberately serializes one
    /// provider conversation while leaving every other session independent.
    state: Mutex<SessionState>,
}

/// Routes fresh inference and explicitly named persistent conversations.
pub struct AgentSessions {
    drivers: HashMap<ProviderKind, Arc<dyn ProviderDriver>>,
    sessions: Mutex<HashMap<SessionKey, Arc<SessionSlot>>>,
}

impl AgentSessions {
    pub fn new(
        drivers: impl IntoIterator<Item = Arc<dyn ProviderDriver>>,
    ) -> Result<Self, AgentError> {
        let mut registered = HashMap::new();
        for driver in drivers {
            registered.insert(driver.provider(), driver);
        }
        Ok(Self {
            drivers: registered,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub async fn run_fresh(
        &self,
        provider: ProviderKind,
        input: AgentInput,
    ) -> Result<AgentResponse, AgentError> {
        self.run_fresh_with_environment(provider, input, InvocationEnvironment::default())
            .await
    }

    pub async fn run_fresh_with_environment(
        &self,
        provider: ProviderKind,
        input: AgentInput,
        environment: InvocationEnvironment,
    ) -> Result<AgentResponse, AgentError> {
        self.driver(provider)?
            .invoke(AgentRequest::fresh(input).with_environment(environment))
            .await
    }

    /// Send one active-hook event to a persistent conversation.
    ///
    /// Upstream callers submit events in source-sequence order. Warden verifies
    /// monotonicity and holds a per-session FIFO mutex across the provider call,
    /// preventing simultaneous resumes from corrupting the conversation.
    pub async fn send_persistent(
        &self,
        key: SessionKey,
        input: AgentInput,
    ) -> Result<AgentResponse, AgentError> {
        self.send_persistent_with_environment(key, input, InvocationEnvironment::default())
            .await
    }

    pub async fn send_persistent_with_environment(
        &self,
        key: SessionKey,
        input: AgentInput,
        environment: InvocationEnvironment,
    ) -> Result<AgentResponse, AgentError> {
        let driver = self.driver(key.provider)?;
        let provider = key.provider;
        let slot = {
            let mut sessions = self.sessions.lock().await;
            Arc::clone(sessions.entry(key).or_default())
        };
        let mut state = slot.state.lock().await;
        ensure_available(&state)?;
        if state.last_successful_source_epoch == input.source_epoch
            && let Some(last) = state.last_successful_source_sequence
            && (input.source_sequence, input.source_ordinal)
                <= (last, state.last_successful_source_ordinal.unwrap_or(0))
        {
            return Err(AgentError::SequenceRegression {
                incoming: input.source_sequence,
                last,
            });
        }

        let response = driver
            .invoke(
                AgentRequest::persistent(input.clone(), state.resume.clone())
                    .with_environment(environment),
            )
            .await?;
        let resume = response
            .resume
            .clone()
            .ok_or(AgentError::MissingResumeMetadata { provider })?;
        if resume.provider != provider {
            return Err(AgentError::ProviderMismatch {
                expected: provider,
                actual: resume.provider,
            });
        }
        state.resume = Some(resume);
        state.last_successful_source_epoch = input.source_epoch;
        state.last_successful_source_sequence = Some(input.source_sequence);
        state.last_successful_source_ordinal = Some(input.source_ordinal);
        Ok(response)
    }

    /// Send one event while coordinating provider advancement with an external durable commit.
    ///
    /// `prepare` runs after sequence validation and before the provider is invoked. It is the
    /// caller's write-ahead boundary. `commit` receives the candidate snapshot after provider
    /// success; Warden publishes that snapshot in memory only after `commit` succeeds. Any
    /// provider or commit failure after `prepare` makes this key unavailable until reset or an
    /// explicit restore.
    pub async fn send_persistent_transactional<Prepare, PrepareFuture, Commit, CommitFuture>(
        &self,
        key: SessionKey,
        input: AgentInput,
        environment: InvocationEnvironment,
        prepare: Prepare,
        commit: Commit,
    ) -> Result<AgentResponse, AgentError>
    where
        Prepare: FnOnce() -> PrepareFuture + Send,
        PrepareFuture: Future<Output = Result<(), String>> + Send,
        Commit: FnOnce(SessionSnapshot) -> CommitFuture + Send,
        CommitFuture: Future<Output = Result<(), String>> + Send,
    {
        let driver = self.driver(key.provider)?;
        let provider = key.provider;
        let slot = {
            let mut sessions = self.sessions.lock().await;
            Arc::clone(sessions.entry(key).or_default())
        };
        let mut state = slot.state.lock().await;
        ensure_available(&state)?;
        if state.last_successful_source_epoch == input.source_epoch
            && let Some(last) = state.last_successful_source_sequence
            && (input.source_sequence, input.source_ordinal)
                <= (last, state.last_successful_source_ordinal.unwrap_or(0))
        {
            return Err(AgentError::SequenceRegression {
                incoming: input.source_sequence,
                last,
            });
        }

        prepare()
            .await
            .map_err(|reason| AgentError::DurablePrepareFailed { reason })?;
        let response = match driver
            .invoke(
                AgentRequest::persistent(input.clone(), state.resume.clone())
                    .with_environment(environment),
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                state.unavailable = Some(format!(
                    "provider invocation failed after its durable pending marker was published: {error}"
                ));
                return Err(error);
            }
        };
        let resume = response
            .resume
            .clone()
            .ok_or(AgentError::MissingResumeMetadata { provider })?;
        if resume.provider != provider {
            state.unavailable = Some(format!(
                "provider returned resume metadata for {}, not {provider}",
                resume.provider
            ));
            return Err(AgentError::ProviderMismatch {
                expected: provider,
                actual: resume.provider,
            });
        }
        let snapshot = SessionSnapshot {
            resume,
            last_successful_source_epoch: input.source_epoch,
            last_successful_source_sequence: Some(input.source_sequence),
            last_successful_source_ordinal: Some(input.source_ordinal),
        };
        if let Err(reason) = commit(snapshot.clone()).await {
            state.unavailable = Some(reason.clone());
            return Err(AgentError::DurableCommitFailed { reason });
        }

        state.resume = Some(snapshot.resume);
        state.last_successful_source_epoch = snapshot.last_successful_source_epoch;
        state.last_successful_source_sequence = snapshot.last_successful_source_sequence;
        state.last_successful_source_ordinal = snapshot.last_successful_source_ordinal;
        Ok(response)
    }

    pub async fn session_snapshot(&self, key: &SessionKey) -> Option<SessionSnapshot> {
        let slot = {
            let sessions = self.sessions.lock().await;
            sessions.get(key).cloned()
        }?;
        let state = slot.state.lock().await;
        if state.unavailable.is_some() {
            return None;
        }
        state.resume.clone().map(|resume| SessionSnapshot {
            resume,
            last_successful_source_epoch: state.last_successful_source_epoch,
            last_successful_source_sequence: state.last_successful_source_sequence,
            last_successful_source_ordinal: state.last_successful_source_ordinal,
        })
    }

    /// Return the current snapshot, distinguishing an absent session from one quarantined after
    /// an outcome-ambiguous provider or durable-storage failure.
    pub async fn session_status(
        &self,
        key: &SessionKey,
    ) -> Result<Option<SessionSnapshot>, AgentError> {
        let slot = {
            let sessions = self.sessions.lock().await;
            sessions.get(key).cloned()
        };
        let Some(slot) = slot else {
            return Ok(None);
        };
        let state = slot.state.lock().await;
        ensure_available(&state)?;
        Ok(state.resume.clone().map(|resume| SessionSnapshot {
            resume,
            last_successful_source_epoch: state.last_successful_source_epoch,
            last_successful_source_sequence: state.last_successful_source_sequence,
            last_successful_source_ordinal: state.last_successful_source_ordinal,
        }))
    }

    pub async fn mark_session_unavailable(
        &self,
        key: SessionKey,
        reason: impl Into<String>,
    ) -> Result<(), AgentError> {
        self.driver(key.provider)?;
        let slot = {
            let mut sessions = self.sessions.lock().await;
            Arc::clone(sessions.entry(key).or_default())
        };
        slot.state.lock().await.unavailable = Some(reason.into());
        Ok(())
    }

    pub async fn restore_session(
        &self,
        key: SessionKey,
        snapshot: SessionSnapshot,
    ) -> Result<(), AgentError> {
        if key.provider != snapshot.resume.provider {
            return Err(AgentError::ProviderMismatch {
                expected: key.provider,
                actual: snapshot.resume.provider,
            });
        }
        self.driver(key.provider)?;
        let slot = Arc::new(SessionSlot {
            state: Mutex::new(SessionState {
                resume: Some(snapshot.resume),
                last_successful_source_epoch: snapshot.last_successful_source_epoch,
                last_successful_source_sequence: snapshot.last_successful_source_sequence,
                last_successful_source_ordinal: snapshot.last_successful_source_ordinal,
                unavailable: None,
            }),
        });
        self.sessions.lock().await.insert(key, slot);
        Ok(())
    }

    pub async fn reset_session(&self, key: &SessionKey) -> bool {
        self.sessions.lock().await.remove(key).is_some()
    }

    pub async fn interrupt(
        &self,
        provider: ProviderKind,
        invocation_id: Uuid,
    ) -> Result<bool, AgentError> {
        self.driver(provider)?.interrupt(invocation_id).await
    }

    pub async fn shutdown(&self) -> Result<(), AgentError> {
        for driver in self.drivers.values() {
            driver.shutdown().await?;
        }
        Ok(())
    }

    fn driver(&self, provider: ProviderKind) -> Result<Arc<dyn ProviderDriver>, AgentError> {
        self.drivers
            .get(&provider)
            .cloned()
            .ok_or(AgentError::DriverUnavailable { provider })
    }
}

fn ensure_available(state: &SessionState) -> Result<(), AgentError> {
    match &state.unavailable {
        Some(reason) => Err(AgentError::SessionUnavailable {
            reason: reason.clone(),
        }),
        None => Ok(()),
    }
}
