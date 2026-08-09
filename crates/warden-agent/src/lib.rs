//! Provider-neutral, supervised Claude Code and Codex CLI sessions.
//!
//! A hook supplies an [`AgentInput`]. Fresh calls never reuse provider context;
//! named persistent calls are serialized and retain only durable provider resume
//! metadata. CLI process lifetime stays an implementation detail.

mod claude;
mod codex;
mod driver;
mod process;
mod sessions;
mod types;

pub use claude::ClaudeCliDriver;
pub use codex::CodexCliDriver;
pub use driver::ProviderDriver;
pub use process::CliConfig;
pub use sessions::AgentSessions;
pub use types::{
    AgentError, AgentInput, AgentRequest, AgentResponse, Conversation, InvocationEnvironment,
    ProviderKind, ResumeMetadata, SessionKey, SessionSnapshot,
};
