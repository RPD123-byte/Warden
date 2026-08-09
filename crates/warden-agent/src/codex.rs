use std::ffi::OsString;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    AgentError, AgentRequest, AgentResponse, CliConfig, Conversation, ProviderDriver, ProviderKind,
    ResumeMetadata,
    process::{CliRunner, parse_jsonl, process_exit_error},
};

/// Codex CLI's non-interactive JSONL adapter.
#[derive(Clone, Debug)]
pub struct CodexCliDriver {
    runner: CliRunner,
    extra_args: Vec<OsString>,
}

impl Default for CodexCliDriver {
    fn default() -> Self {
        Self::new(CliConfig::new("codex"))
    }
}

impl CodexCliDriver {
    #[must_use]
    pub fn new(config: CliConfig) -> Self {
        Self {
            runner: CliRunner::new(ProviderKind::Codex, config),
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
impl ProviderDriver for CodexCliDriver {
    fn provider(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    async fn invoke(&self, request: AgentRequest) -> Result<AgentResponse, AgentError> {
        let message = request.input.user_message()?;
        let (args, fallback_resume, persistent) = self.arguments(&request)?;
        let output = self
            .runner
            .run(request.invocation_id, args, message, &request.environment)
            .await?;
        if !output.status.success() {
            return Err(process_exit_error(self.provider(), output));
        }

        let events = parse_jsonl(self.provider(), &output.stdout)?;
        let parsed = parse_codex_events(&events)?;
        let resume =
            if persistent {
                let session_id = parsed.thread_id.or(fallback_resume).ok_or(
                    AgentError::MissingResumeMetadata {
                        provider: ProviderKind::Codex,
                    },
                )?;
                Some(ResumeMetadata::new(ProviderKind::Codex, session_id))
            } else {
                None
            };

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

impl CodexCliDriver {
    fn arguments(
        &self,
        request: &AgentRequest,
    ) -> Result<(Vec<OsString>, Option<String>, bool), AgentError> {
        match &request.conversation {
            Conversation::Fresh => {
                let mut args = vec!["exec".into(), "--json".into(), "--ephemeral".into()];
                args.extend(self.extra_args.iter().cloned());
                args.extend(["--skip-git-repo-check".into(), "-".into()]);
                Ok((args, None, false))
            }
            Conversation::Persistent { resume: None } => {
                let mut args = vec!["exec".into(), "--json".into()];
                args.extend(self.extra_args.iter().cloned());
                args.extend(["--skip-git-repo-check".into(), "-".into()]);
                Ok((args, None, true))
            }
            Conversation::Persistent {
                resume: Some(resume),
            } => {
                ensure_provider(resume, ProviderKind::Codex)?;
                let mut args = vec!["exec".into(), "resume".into(), "--json".into()];
                args.extend(self.extra_args.iter().cloned());
                args.extend([
                    "--skip-git-repo-check".into(),
                    resume.session_id.clone().into(),
                    "-".into(),
                ]);
                Ok((args, Some(resume.session_id.clone()), true))
            }
        }
    }
}

struct ParsedCodex {
    thread_id: Option<String>,
    text: Option<String>,
    structured_output: Option<Value>,
    usage: Option<Value>,
}

fn parse_codex_events(events: &[Value]) -> Result<ParsedCodex, AgentError> {
    let mut parsed = ParsedCodex {
        thread_id: None,
        text: None,
        structured_output: None,
        usage: None,
    };

    for event in events {
        match event.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                parsed.thread_id = event
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("item.completed") => {
                let Some(item) = event.get("item") else {
                    continue;
                };
                if item.get("type").and_then(Value::as_str) == Some("agent_message") {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        parsed.text = Some(text.to_owned());
                        parsed.structured_output = serde_json::from_str(text).ok();
                    }
                    if let Some(output) = item.get("structured_output") {
                        parsed.structured_output = Some(output.clone());
                    }
                }
            }
            Some("turn.completed") => {
                parsed.usage = event.get("usage").cloned();
            }
            Some("error" | "turn.failed") => {
                let message = event
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| event.pointer("/error/message").and_then(Value::as_str))
                    .unwrap_or("Codex returned a failure event")
                    .to_owned();
                return Err(AgentError::ProviderFailure {
                    provider: ProviderKind::Codex,
                    message,
                });
            }
            _ => {}
        }
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
