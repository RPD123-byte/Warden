//! Warden host daemon: turn-scoped, skill-activated automation for live Codex tasks.

pub mod action;
pub mod activation;
pub mod config;
pub mod event;
pub mod native_hook;
pub mod onboarding;
pub mod python;
pub mod registry;
pub mod runtime;

pub use action::{ActionGrant, ActionKind, GatewayRequest, GatewayResponse};
pub use activation::{ActivationRecord, ActivationRouter, HookDelivery};
pub use config::{Config, DataPaths};
pub use event::{HookEvent, HookEventKind, normalize_event};
pub use native_hook::{
    NativeHookInstall, ensure_native_bridge_bundle, remove_native_bridge_entries,
};
pub use onboarding::{CodexOnboarding, HookTemplateInstall, reconcile_codex};
pub use registry::{HookId, HookRegistry, HookRevision, MARKER_BODY};
pub use runtime::Warden;
