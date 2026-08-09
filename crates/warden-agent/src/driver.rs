use async_trait::async_trait;
use uuid::Uuid;

use crate::{AgentError, AgentRequest, AgentResponse, ProviderKind};

/// Provider-neutral boundary around locally authenticated agent CLIs.
#[async_trait]
pub trait ProviderDriver: Send + Sync {
    fn provider(&self) -> ProviderKind;

    async fn invoke(&self, request: AgentRequest) -> Result<AgentResponse, AgentError>;

    /// Interrupt one currently running invocation.
    ///
    /// Returns `false` when this driver has no matching active invocation.
    async fn interrupt(&self, invocation_id: Uuid) -> Result<bool, AgentError>;

    /// Interrupt every invocation owned by this driver and release runtime state.
    async fn shutdown(&self) -> Result<(), AgentError>;
}
