use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt,
    path::PathBuf,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

/// A locally supported inference provider.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Claude,
    Codex,
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claude => formatter.write_str("claude"),
            Self::Codex => formatter.write_str("codex"),
        }
    }
}

/// The input Warden sends to an agent module.
///
/// `event` is retained unchanged. [`AgentInput::user_message`] serializes the
/// complete value into the provider user message automatically.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentInput {
    /// Identifies the source-ingress run whose sequence space produced this event.
    /// Sequence numbers are only monotonic within one epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_epoch: Option<Uuid>,
    pub source_sequence: u64,
    /// Stable order among multiple normalized Warden events derived from one source frame.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub source_ordinal: u16,
    pub event: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standing_prompt: Option<String>,
}

impl AgentInput {
    #[must_use]
    pub fn new(source_sequence: u64, event: Value) -> Self {
        Self {
            source_epoch: None,
            source_sequence,
            source_ordinal: 0,
            event,
            standing_prompt: None,
        }
    }

    #[must_use]
    pub fn with_source_epoch(mut self, source_epoch: Uuid) -> Self {
        self.source_epoch = Some(source_epoch);
        self
    }

    #[must_use]
    pub fn with_source_ordinal(mut self, source_ordinal: u16) -> Self {
        self.source_ordinal = source_ordinal;
        self
    }

    #[must_use]
    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        let prompt = prompt.into();
        self.standing_prompt = (!prompt.trim().is_empty()).then_some(prompt);
        self
    }

    /// Render the provider user message using Warden's default behavior.
    ///
    /// With no standing prompt this is exactly the serialized event. When a
    /// prompt is present, it accompanies that complete JSON value without
    /// transforming or selectively projecting the event.
    pub fn user_message(&self) -> Result<String, AgentError> {
        let event = serde_json::to_string(&self.event).map_err(AgentError::SerializeEvent)?;
        Ok(match self.standing_prompt.as_deref() {
            Some(prompt) => format!("{prompt}\n\nIncoming Warden hook event JSON:\n{event}"),
            None => event,
        })
    }
}

/// Durable provider metadata needed to resume one conversation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResumeMetadata {
    pub provider: ProviderKind,
    pub session_id: String,
    /// Reserved for provider-specific durable metadata without changing the
    /// common record format.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, Value>,
}

impl ResumeMetadata {
    #[must_use]
    pub fn new(provider: ProviderKind, session_id: impl Into<String>) -> Self {
        Self {
            provider,
            session_id: session_id.into(),
            details: BTreeMap::new(),
        }
    }
}

/// Whether a provider call starts without context or resumes a named session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Conversation {
    Fresh,
    Persistent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<ResumeMetadata>,
    },
}

/// Per-invocation child-process environment, such as Warden's local action
/// socket and short-lived credential. Values are neither serialized nor shown
/// by `Debug`.
#[derive(Clone, Default)]
pub struct InvocationEnvironment(BTreeMap<OsString, OsString>);

impl InvocationEnvironment {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_var(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }

    pub fn insert(
        &mut self,
        key: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Option<OsString> {
        self.0.insert(key.into(), value.into())
    }

    #[must_use]
    pub fn get(&self, key: impl AsRef<OsStr>) -> Option<&OsStr> {
        self.0.get(key.as_ref()).map(OsString::as_os_str)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&OsString, &OsString)> {
        self.0.iter()
    }
}

impl fmt::Debug for InvocationEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InvocationEnvironment")
            .field("keys", &self.0.keys().collect::<Vec<_>>())
            .field("values", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct AgentRequest {
    pub invocation_id: Uuid,
    pub input: AgentInput,
    pub conversation: Conversation,
    pub model: Option<String>,
    pub environment: InvocationEnvironment,
}

impl AgentRequest {
    #[must_use]
    pub fn fresh(input: AgentInput) -> Self {
        Self {
            invocation_id: Uuid::new_v4(),
            input,
            conversation: Conversation::Fresh,
            model: None,
            environment: InvocationEnvironment::default(),
        }
    }

    #[must_use]
    pub fn persistent(input: AgentInput, resume: Option<ResumeMetadata>) -> Self {
        Self {
            invocation_id: Uuid::new_v4(),
            input,
            conversation: Conversation::Persistent { resume },
            model: None,
            environment: InvocationEnvironment::default(),
        }
    }

    #[must_use]
    pub fn with_environment(mut self, environment: InvocationEnvironment) -> Self {
        self.environment = environment;
        self
    }

    #[must_use]
    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }
}

/// Structured result retained from a provider's JSON/JSONL output.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentResponse {
    pub provider: ProviderKind,
    pub invocation_id: Uuid,
    pub source_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
    pub events: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumeMetadata>,
}

/// Stable identity for one explicitly persistent provider conversation.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct SessionKey {
    pub provider: ProviderKind,
    pub hook_id: String,
    pub session_name: String,
    pub source_thread_id: String,
}

impl SessionKey {
    #[must_use]
    pub fn new(
        provider: ProviderKind,
        hook_id: impl Into<String>,
        session_name: impl Into<String>,
        source_thread_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            hook_id: hook_id.into(),
            session_name: session_name.into(),
            source_thread_id: source_thread_id.into(),
        }
    }
}

/// Serializable state the daemon can persist and restore.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionSnapshot {
    pub resume: ResumeMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_source_epoch: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_source_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_source_ordinal: Option<u16>,
}

const fn is_zero(value: &u16) -> bool {
    *value == 0
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("failed to serialize the incoming hook event: {0}")]
    SerializeEvent(#[source] serde_json::Error),

    #[error("no {provider} provider driver is registered")]
    DriverUnavailable { provider: ProviderKind },

    #[error("resume metadata is for {actual}, not {expected}")]
    ProviderMismatch {
        expected: ProviderKind,
        actual: ProviderKind,
    },

    #[error("persistent {provider} call did not return resumable session metadata")]
    MissingResumeMetadata { provider: ProviderKind },

    #[error("persistent session model {requested:?} does not match its bound model {bound:?}")]
    SessionModelMismatch {
        requested: Option<String>,
        bound: Option<String>,
    },

    #[error(
        "source sequence {incoming} is not newer than the session's last successful sequence {last}"
    )]
    SequenceRegression { incoming: u64, last: u64 },

    #[error(
        "persistent session is unavailable until it is explicitly reset or recovered: {reason}"
    )]
    SessionUnavailable { reason: String },

    #[error("could not durably begin the persistent-session transaction: {reason}")]
    DurablePrepareFailed { reason: String },

    #[error(
        "persistent-session durable commit failed; the session is unavailable until reset or recovery: {reason}"
    )]
    DurableCommitFailed { reason: String },

    #[error("failed to start {provider} CLI at {program}: {source}")]
    Spawn {
        provider: ProviderKind,
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write the {provider} CLI input: {source}")]
    WriteInput {
        provider: ProviderKind,
        #[source]
        source: std::io::Error,
    },

    #[error("failed while waiting for the {provider} CLI: {source}")]
    Wait {
        provider: ProviderKind,
        #[source]
        source: std::io::Error,
    },

    #[error("{provider} CLI invocation exceeded its {timeout:?} timeout")]
    Timeout {
        provider: ProviderKind,
        timeout: Duration,
    },

    #[error("{provider} CLI invocation was interrupted")]
    Interrupted { provider: ProviderKind },

    #[error("{provider} CLI input was {actual} bytes; the limit is {limit} bytes")]
    InputTooLarge {
        provider: ProviderKind,
        actual: usize,
        limit: usize,
    },

    #[error("{provider} CLI {stream} exceeded its {limit}-byte capture limit")]
    OutputTooLarge {
        provider: ProviderKind,
        stream: &'static str,
        limit: usize,
    },

    #[error("{provider} CLI exited with status {code:?}: {stderr}")]
    ProcessExit {
        provider: ProviderKind,
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },

    #[error("{provider} emitted invalid JSONL on line {line}: {source}")]
    InvalidJsonLine {
        provider: ProviderKind,
        line: usize,
        text: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("{provider} emitted no structured output")]
    EmptyOutput { provider: ProviderKind },

    #[error("{provider} reported an inference failure: {message}")]
    ProviderFailure {
        provider: ProviderKind,
        message: String,
    },

    #[error("provider invocation registry is unavailable")]
    InvocationRegistryPoisoned,

    #[error("{provider} provider is shutting down and cannot accept another invocation")]
    ShuttingDown { provider: ProviderKind },

    #[error("{provider} provider shutdown exceeded its {timeout:?} cleanup deadline")]
    ShutdownTimeout {
        provider: ProviderKind,
        timeout: Duration,
    },

    #[error("provider process task failed: {0}")]
    ProcessTask(#[from] tokio::task::JoinError),
}
