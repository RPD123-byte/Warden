use std::ffi::OsString;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    AgentError, AgentRequest, AgentResponse, CliConfig, Conversation, ProviderDriver, ProviderKind,
    ResumeMetadata,
    process::{CliRunner, parse_jsonl, process_exit_error},
};

/// Claude Code's non-interactive structured-output adapter.
#[derive(Clone, Debug)]
pub struct ClaudeCliDriver {
    runner: CliRunner,
    extra_args: Vec<OsString>,
}

impl Default for ClaudeCliDriver {
    fn default() -> Self {
        Self::new(CliConfig::new("claude"))
    }
}

impl ClaudeCliDriver {
    #[must_use]
    pub fn new(config: CliConfig) -> Self {
        Self {
            runner: CliRunner::new(ProviderKind::Claude, config),
            extra_args: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_extra_arg(mut self, arg: impl Into<OsString>) -> Self {
        self.extra_args.push(arg.into());
        self
    }
}

#[async_trait]
impl ProviderDriver for ClaudeCliDriver {
    fn provider(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    async fn invoke(&self, request: AgentRequest) -> Result<AgentResponse, AgentError> {
        let message = request.input.user_message()?;
        let (args, fallback_session_id, persistent) = self.arguments(&request)?;
        let output = self
            .runner
            .run(request.invocation_id, args, message, &request.environment)
            .await?;
        if !output.status.success() {
            return Err(process_exit_error(self.provider(), output));
        }

        let events = parse_jsonl(self.provider(), &output.stdout)?;
        let parsed = parse_claude_events(&events)?;
        let resume = persistent.then(|| {
            ResumeMetadata::new(
                ProviderKind::Claude,
                parsed
                    .session_id
                    .or(fallback_session_id)
                    .expect("a new persistent Claude invocation always supplies a UUID fallback"),
            )
        });

        Ok(AgentResponse {
            provider: self.provider(),
            invocation_id: request.invocation_id,
            source_sequence: request.input.source_sequence,
            text: parsed.text,
            structured_output: parsed.structured_output,
            events,
            usage: parsed.usage,
            resume,
        })
    }

    async fn interrupt(&self, invocation_id: Uuid) -> Result<bool, AgentError> {
        self.runner.interrupt(invocation_id)
    }

    async fn shutdown(&self) -> Result<(), AgentError> {
        self.runner.shutdown().await
    }
}

impl ClaudeCliDriver {
    fn arguments(
        &self,
        request: &AgentRequest,
    ) -> Result<(Vec<OsString>, Option<String>, bool), AgentError> {
        let mut args = vec![
            "--print".into(),
            "--input-format".into(),
            "text".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
        ];
        args.extend(self.extra_args.iter().cloned());

        match &request.conversation {
            Conversation::Fresh => {
                args.push("--no-session-persistence".into());
                Ok((args, None, false))
            }
            Conversation::Persistent {
                resume: Some(resume),
            } => {
                ensure_provider(resume, ProviderKind::Claude)?;
                args.push("--resume".into());
                args.push(resume.session_id.clone().into());
                Ok((args, Some(resume.session_id.clone()), true))
            }
            Conversation::Persistent { resume: None } => {
                let session_id = Uuid::new_v4().to_string();
                args.push("--session-id".into());
                args.push(session_id.clone().into());
                Ok((args, Some(session_id), true))
            }
        }
    }
}

struct ParsedClaude {
    session_id: Option<String>,
    text: Option<String>,
    structured_output: Option<Value>,
    usage: Option<Value>,
}

fn parse_claude_events(events: &[Value]) -> Result<ParsedClaude, AgentError> {
    let mut parsed = ParsedClaude {
        session_id: None,
        text: None,
        structured_output: None,
        usage: None,
    };

    for event in events {
        if let Some(session_id) = event.get("session_id").and_then(Value::as_str) {
            parsed.session_id = Some(session_id.to_owned());
        }
        if event.get("type").and_then(Value::as_str) != Some("result") {
            continue;
        }
        if event.get("is_error").and_then(Value::as_bool) == Some(true)
            || event
                .get("subtype")
                .and_then(Value::as_str)
                .is_some_and(|subtype| !matches!(subtype, "success" | "completed"))
        {
            let message = event
                .get("result")
                .and_then(Value::as_str)
                .or_else(|| event.get("error").and_then(Value::as_str))
                .unwrap_or("Claude returned an unsuccessful result")
                .to_owned();
            return Err(AgentError::ProviderFailure {
                provider: ProviderKind::Claude,
                message,
            });
        }
        if let Some(result) = event.get("result") {
            if let Some(text) = result.as_str() {
                parsed.text = Some(text.to_owned());
                parsed.structured_output = serde_json::from_str(text).ok();
            } else if !result.is_null() {
                parsed.structured_output = Some(result.clone());
            }
        }
        if let Some(structured) = event.get("structured_output") {
            parsed.structured_output = Some(structured.clone());
        }
        parsed.usage = event.get("usage").cloned();
    }

    Ok(parsed)
}

fn ensure_provider(resume: &ResumeMetadata, expected: ProviderKind) -> Result<(), AgentError> {
    if resume.provider != expected {
        return Err(AgentError::ProviderMismatch {
            expected,
            actual: resume.provider,
        });
    }
    Ok(())
}
