use crate::{
    activation::{ActivationRouter, HookDelivery},
    event::{HookEvent, HookEventEnvelope, HookEventKind, NativeHookContext},
    python::PythonRuntime,
    registry::HookId,
};
use async_trait::async_trait;
use codex_control::{ActionOutcome, Handle};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{File, OpenOptions},
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, RwLock, Semaphore, watch},
    task::JoinSet,
};
use uuid::Uuid;

pub const ACTION_PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST_ID_BYTES: usize = 128;
const NON_BLOCKING_QUEUE_MULTIPLIER: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    CurrentEvent,
    CurrentThreadSnapshot,
    CurrentThreadHistory,
    TurnStart,
    TurnSteer,
    TurnInterrupt,
    ThreadList,
    ArbitraryThreadSnapshot,
    ArbitraryThreadHistory,
    ArbitraryTurnStart,
    ArbitraryTurnSteer,
    ArbitraryTurnInterrupt,
}

impl ActionKind {
    pub fn parse(value: &str) -> Result<Self, ActionError> {
        serde_json::from_value(Value::String(value.to_owned()))
            .map_err(|_| ActionError::UnknownAction(value.to_owned()))
    }

    pub fn is_cross_thread(self) -> bool {
        matches!(
            self,
            Self::ThreadList
                | Self::ArbitraryThreadSnapshot
                | Self::ArbitraryThreadHistory
                | Self::ArbitraryTurnStart
                | Self::ArbitraryTurnSteer
                | Self::ArbitraryTurnInterrupt
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionGrant {
    pub actions: HashSet<ActionKind>,
}

impl ActionGrant {
    pub fn from_names<'a>(
        names: impl IntoIterator<Item = &'a String>,
    ) -> Result<Self, ActionError> {
        Ok(Self {
            actions: names
                .into_iter()
                .map(|name| ActionKind::parse(name))
                .collect::<Result<_, _>>()?,
        })
    }

    pub fn allows(&self, action: ActionKind) -> bool {
        self.actions.contains(&action)
    }
}

#[derive(Clone, Debug)]
pub struct InvocationContext {
    pub invocation_id: Uuid,
    pub token: String,
    pub hook_id: HookId,
    pub thread_id: String,
    pub turn_id: String,
    pub event: HookEventEnvelope,
    pub grant: ActionGrant,
    cancellation: watch::Sender<bool>,
    expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvocationCredential {
    pub invocation_id: Uuid,
    pub token: String,
    pub socket: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayRequest {
    #[serde(rename = "type")]
    pub message_type: String,
    pub protocol_version: u32,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub context: Option<RequestCredential>,
    #[serde(default)]
    pub bridge_auth: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestCredential {
    pub invocation_id: Uuid,
    pub token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayResponse {
    #[serde(rename = "type")]
    pub message_type: String,
    pub protocol_version: u32,
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<GatewayErrorBody>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("unknown Warden action {0:?}")]
    UnknownAction(String),
    #[error("missing invocation credential")]
    MissingCredential,
    #[error("invocation credential is invalid or expired")]
    InvalidCredential,
    #[error("action {0:?} was not granted to this hook revision")]
    AccessDenied(ActionKind),
    #[error("current-scope action cannot target thread {requested:?}; it is bound to {bound:?}")]
    TargetScope { requested: String, bound: String },
    #[error("request parameter {0:?} is missing or invalid")]
    InvalidParameter(&'static str),
    #[error("agent service failed: {0}")]
    Agent(String),
    #[error("hook invocation was revoked while its agent call was active")]
    InvocationCancelled,
    #[error("gateway protocol error: {0}")]
    Protocol(String),
    #[error("gateway I/O error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug)]
pub struct AgentCallContext {
    pub hook_id: HookId,
    pub source_thread_id: String,
    pub event: HookEventEnvelope,
    pub credential: InvocationCredential,
    pub grant: ActionGrant,
}

#[async_trait]
pub trait AgentBackend: Send + Sync + 'static {
    async fn run_fresh(
        &self,
        context: AgentCallContext,
        provider: &str,
        prompt: Option<String>,
        model: Option<String>,
    ) -> Result<Value, String>;

    async fn send_persistent(
        &self,
        context: AgentCallContext,
        provider: &str,
        session_name: &str,
        prompt: Option<String>,
        model: Option<String>,
    ) -> Result<Value, String>;

    async fn reset(
        &self,
        context: AgentCallContext,
        provider: &str,
        session_name: &str,
    ) -> Result<Value, String>;

    async fn status(
        &self,
        context: AgentCallContext,
        provider: &str,
        session_name: &str,
    ) -> Result<Value, String>;
}

pub struct NoAgentBackend;

#[async_trait]
impl AgentBackend for NoAgentBackend {
    async fn run_fresh(
        &self,
        _: AgentCallContext,
        _: &str,
        _: Option<String>,
        _: Option<String>,
    ) -> Result<Value, String> {
        Err("agent providers are not configured".into())
    }
    async fn send_persistent(
        &self,
        _: AgentCallContext,
        _: &str,
        _: &str,
        _: Option<String>,
        _: Option<String>,
    ) -> Result<Value, String> {
        Err("agent providers are not configured".into())
    }
    async fn reset(&self, _: AgentCallContext, _: &str, _: &str) -> Result<Value, String> {
        Err("agent providers are not configured".into())
    }
    async fn status(&self, _: AgentCallContext, _: &str, _: &str) -> Result<Value, String> {
        Err("agent providers are not configured".into())
    }
}

#[derive(Clone)]
pub struct ActionGateway {
    handle: Handle,
    socket_path: PathBuf,
    credential_ttl: Duration,
    invocations: Arc<RwLock<HashMap<Uuid, InvocationContext>>>,
    agents: Arc<dyn AgentBackend>,
    daemon_health: Arc<RwLock<Value>>,
    native: Option<NativeHookRuntime>,
    invocation_capacity: Arc<Semaphore>,
    non_blocking_capacity: Arc<Semaphore>,
    non_blocking_tasks: Arc<Mutex<JoinSet<()>>>,
    queued_non_blocking: Arc<AtomicUsize>,
    active_non_blocking: Arc<AtomicUsize>,
    non_blocking_limit: usize,
    rejected_non_blocking: Arc<AtomicU64>,
}

#[derive(Clone)]
struct NativeHookRuntime {
    router: ActivationRouter,
    python: Arc<PythonRuntime>,
    bridge_token: Arc<str>,
    receipt_ordinal: Arc<AtomicU64>,
}

struct InvocationRevocationGuard {
    invocation_id: Uuid,
    invocations: Arc<RwLock<HashMap<Uuid, InvocationContext>>>,
    armed: bool,
}

impl InvocationRevocationGuard {
    fn new(
        invocation_id: Uuid,
        invocations: Arc<RwLock<HashMap<Uuid, InvocationContext>>>,
    ) -> Self {
        Self {
            invocation_id,
            invocations,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InvocationRevocationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let invocation_id = self.invocation_id;
        let invocations = self.invocations.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(context) = invocations.write().await.remove(&invocation_id) {
                    let _ = context.cancellation.send(true);
                }
            });
        }
    }
}

impl ActionGateway {
    pub fn new(handle: Handle, socket_path: PathBuf, agents: Arc<dyn AgentBackend>) -> Self {
        Self {
            handle,
            socket_path,
            credential_ttl: Duration::from_secs(15 * 60),
            invocations: Arc::new(RwLock::new(HashMap::new())),
            agents,
            daemon_health: Arc::new(RwLock::new(json!({}))),
            native: None,
            invocation_capacity: Arc::new(Semaphore::new(16)),
            non_blocking_capacity: Arc::new(Semaphore::new(64)),
            non_blocking_tasks: Arc::new(Mutex::new(JoinSet::new())),
            queued_non_blocking: Arc::new(AtomicUsize::new(0)),
            active_non_blocking: Arc::new(AtomicUsize::new(0)),
            non_blocking_limit: 64,
            rejected_non_blocking: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn with_native_hook_runtime(
        mut self,
        router: ActivationRouter,
        python: Arc<PythonRuntime>,
        bridge_token: String,
        max_concurrent_hooks: usize,
    ) -> Self {
        self.native = Some(NativeHookRuntime {
            router,
            python,
            bridge_token: bridge_token.into(),
            receipt_ordinal: Arc::new(AtomicU64::new(1)),
        });
        let concurrent = max_concurrent_hooks.max(1);
        let outstanding = concurrent.saturating_mul(NON_BLOCKING_QUEUE_MULTIPLIER);
        self.invocation_capacity = Arc::new(Semaphore::new(concurrent));
        self.non_blocking_capacity = Arc::new(Semaphore::new(outstanding));
        self.non_blocking_limit = outstanding;
        self
    }

    pub async fn set_daemon_health(&self, health: Value) {
        *self.daemon_health.write().await = health;
    }

    pub async fn merge_daemon_health(&self, health: Value) {
        let mut current = self.daemon_health.write().await;
        let current = current
            .as_object_mut()
            .expect("daemon health is always an object");
        if let Some(update) = health.as_object() {
            current.extend(update.clone());
        }
    }

    pub async fn register_invocation(
        &self,
        hook_id: HookId,
        event: HookEventEnvelope,
        grant: ActionGrant,
    ) -> Result<InvocationCredential, ActionError> {
        let thread_id = event
            .thread_id
            .clone()
            .ok_or(ActionError::InvalidParameter("event.thread_id"))?;
        let turn_id = event
            .turn_id
            .clone()
            .ok_or(ActionError::InvalidParameter("event.turn_id"))?;
        let invocation_id = Uuid::new_v4();
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let expires_at_unix_ms = now_unix_ms()
            .saturating_add(u64::try_from(self.credential_ttl.as_millis()).unwrap_or(u64::MAX));
        let (cancellation, _) = watch::channel(false);
        self.invocations.write().await.insert(
            invocation_id,
            InvocationContext {
                invocation_id,
                token: token.clone(),
                hook_id,
                thread_id,
                turn_id,
                event,
                grant,
                cancellation,
                expires_at_unix_ms,
            },
        );
        Ok(InvocationCredential {
            invocation_id,
            token,
            socket: self.socket_path.clone(),
        })
    }

    pub async fn revoke_invocation(&self, invocation_id: Uuid) {
        if let Some(context) = self.invocations.write().await.remove(&invocation_id) {
            let _ = context.cancellation.send(true);
        }
    }

    pub async fn dispatch(&self, request: GatewayRequest) -> GatewayResponse {
        let id = request.id.clone();
        let result = self.dispatch_inner(request).await;
        match result {
            Ok(result) => GatewayResponse {
                message_type: "response".into(),
                protocol_version: ACTION_PROTOCOL_VERSION,
                id,
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(error) => GatewayResponse {
                message_type: "response".into(),
                protocol_version: ACTION_PROTOCOL_VERSION,
                id,
                ok: false,
                result: None,
                error: Some(GatewayErrorBody {
                    code: error_code(&error).into(),
                    message: error.to_string(),
                }),
            },
        }
    }

    async fn dispatch_inner(&self, request: GatewayRequest) -> Result<Value, ActionError> {
        if request.message_type != "request" || request.protocol_version != ACTION_PROTOCOL_VERSION
        {
            return Err(ActionError::Protocol(format!(
                "expected request protocol version {ACTION_PROTOCOL_VERSION}"
            )));
        }
        if request.method == "warden.health" {
            let health = self.handle.health().borrow().clone();
            return Ok(json!({
                "phase": format!("{:?}", health.phase).to_lowercase(),
                "reconnect_attempts": health.reconnect_attempts,
                "last_frame_sequence": health.last_frame_sequence,
                "detail": health.detail,
                "active_invocations": self.invocations.read().await.len(),
                "dispatcher": self.dispatcher_diagnostics(),
                "daemon": self.daemon_health.read().await.clone(),
            }));
        }
        if request.method == "warden.native_hook.event" {
            return self
                .dispatch_native_event(request.params, request.bridge_auth.as_deref())
                .await;
        }
        let context = self.authenticate(request.context.as_ref()).await?;
        match request.method.as_str() {
            "warden.action" => {
                let name = string_param(&request.params, "name")?;
                let arguments = request
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                self.execute_action(&context, ActionKind::parse(name)?, &arguments)
                    .await
            }
            "agent.run" => {
                validate_agent_inference_params(&context, &request.params)?;
                let provider = string_param(&request.params, "provider")?;
                let prompt = optional_string_param(&request.params, "prompt")?;
                let model = optional_non_empty_string_param(&request.params, "model")?;
                await_agent_call(
                    &context,
                    self.agents
                        .run_fresh(self.agent_context(&context), provider, prompt, model),
                )
                .await
            }
            "agent.session.send" => {
                validate_agent_inference_params(&context, &request.params)?;
                let provider = string_param(&request.params, "provider")?;
                let name = string_param(&request.params, "name")?;
                let prompt = optional_string_param(&request.params, "prompt")?;
                let model = optional_non_empty_string_param(&request.params, "model")?;
                await_agent_call(
                    &context,
                    self.agents.send_persistent(
                        self.agent_context(&context),
                        provider,
                        name,
                        prompt,
                        model,
                    ),
                )
                .await
            }
            "agent.session.reset" => {
                let provider = string_param(&request.params, "provider")?;
                let name = string_param(&request.params, "name")?;
                self.agents
                    .reset(self.agent_context(&context), provider, name)
                    .await
                    .map_err(ActionError::Agent)
            }
            "agent.session.status" => {
                let provider = string_param(&request.params, "provider")?;
                let name = string_param(&request.params, "name")?;
                self.agents
                    .status(self.agent_context(&context), provider, name)
                    .await
                    .map_err(ActionError::Agent)
            }
            other => Err(ActionError::Protocol(format!(
                "unsupported method {other:?}"
            ))),
        }
    }

    async fn dispatch_native_event(
        &self,
        params: Value,
        bridge_auth: Option<&str>,
    ) -> Result<Value, ActionError> {
        let runtime = self
            .native
            .as_ref()
            .ok_or_else(|| ActionError::Protocol("native hook runtime is not configured".into()))?;
        let supplied = bridge_auth.ok_or(ActionError::MissingCredential)?;
        if !constant_time_equal(runtime.bridge_token.as_bytes(), supplied.as_bytes()) {
            return Err(ActionError::InvalidCredential);
        }
        let event_name = string_param(&params, "hook_event_name")?.to_owned();
        let thread_id = string_param(&params, "session_id")?.to_owned();
        let turn_id = string_param(&params, "turn_id")?.to_owned();
        {
            let mut health = self.daemon_health.write().await;
            if let Some(bridge) = health.get_mut("bridge").and_then(Value::as_object_mut) {
                bridge.insert("loaded_confirmed".into(), Value::Bool(true));
                let loaded = bridge
                    .entry("loaded_tasks")
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .expect("loaded_tasks is initialized as an array");
                if !loaded
                    .iter()
                    .any(|value| value.as_str() == Some(&thread_id))
                {
                    loaded.push(Value::String(thread_id.clone()));
                    loaded.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
                }
            }
        }
        let ordinal = runtime.receipt_ordinal.fetch_add(1, Ordering::AcqRel);
        let received = now_unix_ms();
        let item_id = params
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let mut events = Vec::new();
        let native_context = || NativeHookContext {
            event_name: event_name.clone(),
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            receipt_ordinal: ordinal,
            unix_receipt_ms: received,
            raw_payload: params.clone(),
        };
        let activated = if event_name == "UserPromptSubmit" {
            let prompt = string_param(&params, "prompt")?;
            let input = json!([{"type":"text", "text": prompt}]);
            let activated = runtime
                .router
                .begin_native_prompt(&thread_id, &turn_id, &input)
                .await;
            events.push(HookEvent::native(
                HookEventKind::UserPromptSubmitted,
                native_context(),
                None,
                input,
            ));
            events.push(HookEvent::native(
                HookEventKind::TurnStarted,
                native_context(),
                None,
                params.clone(),
            ));
            activated
        } else {
            match event_name.as_str() {
                "PreToolUse" => events.push(HookEvent::native(
                    HookEventKind::PreToolUse,
                    native_context(),
                    item_id,
                    params.clone(),
                )),
                "PostToolUse" => events.push(HookEvent::native(
                    HookEventKind::PostToolUse,
                    native_context(),
                    item_id,
                    params.clone(),
                )),
                "Stop" => {
                    if params
                        .get("last_assistant_message")
                        .is_some_and(|value| value.is_string())
                    {
                        events.push(HookEvent::native(
                            HookEventKind::AgentMessageCompleted,
                            native_context(),
                            None,
                            json!({"type":"agentMessage", "text": params["last_assistant_message"]}),
                        ));
                    }
                }
                _ => return Err(ActionError::InvalidParameter("hook_event_name")),
            }
            Vec::new()
        };
        let mut deliveries = Vec::new();
        for event in events {
            deliveries.extend(runtime.router.route(event).await);
        }
        let (blocking, non_blocking, rejected) = self
            .dispatch_deliveries(runtime.python.clone(), deliveries)
            .await;
        Ok(json!({
            "activated": activated.into_iter().map(|id| id.to_string()).collect::<Vec<_>>(),
            "blocking": blocking,
            "non_blocking": non_blocking,
            "rejected_non_blocking": rejected,
        }))
    }

    pub async fn dispatch_deliveries(
        &self,
        python: Arc<PythonRuntime>,
        deliveries: Vec<HookDelivery>,
    ) -> (usize, usize, usize) {
        let (blocking, non_blocking): (Vec<_>, Vec<_>) = deliveries
            .into_iter()
            .partition(|delivery| delivery.revision.metadata.blocking);
        self.reap_non_blocking().await;
        let mut rejected = 0;
        for delivery in non_blocking.iter().cloned() {
            let Ok(queue_slot) = self.non_blocking_capacity.clone().try_acquire_owned() else {
                rejected += 1;
                self.rejected_non_blocking.fetch_add(1, Ordering::AcqRel);
                tracing::warn!(hook = %delivery.revision.id, "non-blocking hook queue saturated; invocation rejected");
                continue;
            };
            let gateway = self.clone();
            let python = python.clone();
            let execution = self.invocation_capacity.clone();
            let queued = self.queued_non_blocking.clone();
            let active = self.active_non_blocking.clone();
            queued.fetch_add(1, Ordering::AcqRel);
            self.non_blocking_tasks.lock().await.spawn(async move {
                let Ok(execution) = execution.acquire_owned().await else {
                    queued.fetch_sub(1, Ordering::AcqRel);
                    return;
                };
                queued.fetch_sub(1, Ordering::AcqRel);
                active.fetch_add(1, Ordering::AcqRel);
                gateway.invoke_delivery(python, delivery).await;
                active.fetch_sub(1, Ordering::AcqRel);
                drop(execution);
                drop(queue_slot);
            });
        }
        let mut tasks = JoinSet::new();
        for delivery in blocking.iter().cloned() {
            let gateway = self.clone();
            let python = python.clone();
            let execution = self.invocation_capacity.clone();
            tasks.spawn(async move {
                let Ok(_permit) = execution.acquire_owned().await else {
                    return;
                };
                gateway.invoke_delivery(python, delivery).await;
            });
        }
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                tracing::error!(%error, "blocking hook invocation task failed");
            }
        }
        (
            blocking.len(),
            non_blocking.len().saturating_sub(rejected),
            rejected,
        )
    }

    fn dispatcher_diagnostics(&self) -> Value {
        json!({
            "non_blocking_queue_limit": self.non_blocking_limit,
            "non_blocking_queued": self.queued_non_blocking.load(Ordering::Acquire),
            "non_blocking_active": self.active_non_blocking.load(Ordering::Acquire),
            "non_blocking_rejected": self.rejected_non_blocking.load(Ordering::Acquire),
        })
    }

    async fn reap_non_blocking(&self) {
        let mut tasks = self.non_blocking_tasks.lock().await;
        while let Some(result) = tasks.try_join_next() {
            if let Err(error) = result {
                tracing::warn!(%error, "non-blocking hook task failed");
            }
        }
    }

    async fn shutdown_non_blocking(&self) {
        let mut tasks = self.non_blocking_tasks.lock().await;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        self.queued_non_blocking.store(0, Ordering::Release);
        self.active_non_blocking.store(0, Ordering::Release);
    }

    pub async fn invoke_delivery(&self, python: Arc<PythonRuntime>, delivery: HookDelivery) {
        let envelope = delivery.event.envelope();
        let grant = match ActionGrant::from_names(delivery.revision.metadata.actions.iter()) {
            Ok(grant) => grant,
            Err(error) => {
                tracing::error!(
                    hook = %delivery.revision.id,
                    revision = %delivery.revision.revision,
                    %error,
                    "hook revision has an invalid action grant"
                );
                return;
            }
        };
        let credential = match self
            .register_invocation(delivery.revision.id.clone(), envelope.clone(), grant)
            .await
        {
            Ok(credential) => credential,
            Err(error) => {
                tracing::error!(hook = %delivery.revision.id, %error, "could not register hook invocation");
                return;
            }
        };
        let invocation_id = credential.invocation_id;
        let mut revocation =
            InvocationRevocationGuard::new(invocation_id, self.invocations.clone());
        let result = python
            .invoke(delivery.revision.clone(), envelope, credential)
            .await;
        self.revoke_invocation(invocation_id).await;
        revocation.disarm();
        match result {
            Ok(result) => {
                if result.logs != Value::Null {
                    tracing::debug!(hook = %delivery.revision.id, invocation = %invocation_id, logs = %result.logs, "hook logs");
                }
            }
            Err(error) => tracing::error!(
                hook = %delivery.revision.id,
                revision = %delivery.revision.revision,
                sequence = delivery.event.receipt_ordinal(),
                invocation = %invocation_id,
                %error,
                "hook invocation failed"
            ),
        }
    }

    async fn authenticate(
        &self,
        credential: Option<&RequestCredential>,
    ) -> Result<InvocationContext, ActionError> {
        let credential = credential.ok_or(ActionError::MissingCredential)?;
        let mut invocations = self.invocations.write().await;
        invocations.retain(|_, value| value.expires_at_unix_ms >= now_unix_ms());
        let context = invocations
            .get(&credential.invocation_id)
            .filter(|context| {
                constant_time_equal(context.token.as_bytes(), credential.token.as_bytes())
            })
            .cloned()
            .ok_or(ActionError::InvalidCredential)?;
        Ok(context)
    }

    fn agent_context(&self, context: &InvocationContext) -> AgentCallContext {
        AgentCallContext {
            hook_id: context.hook_id.clone(),
            source_thread_id: context.thread_id.clone(),
            event: context.event.clone(),
            credential: InvocationCredential {
                invocation_id: context.invocation_id,
                token: context.token.clone(),
                socket: self.socket_path.clone(),
            },
            grant: context.grant.clone(),
        }
    }

    async fn execute_action(
        &self,
        context: &InvocationContext,
        action: ActionKind,
        arguments: &Value,
    ) -> Result<Value, ActionError> {
        if !context.grant.allows(action) {
            return Err(ActionError::AccessDenied(action));
        }
        if !action.is_cross_thread()
            && let Some(requested) = arguments.get("thread_id").and_then(Value::as_str)
            && requested != context.thread_id
        {
            return Err(ActionError::TargetScope {
                requested: requested.to_owned(),
                bound: context.thread_id.clone(),
            });
        }
        match action {
            ActionKind::CurrentEvent => serde_json::to_value(&context.event)
                .map_err(|error| ActionError::Protocol(error.to_string())),
            ActionKind::CurrentThreadSnapshot => self.thread_snapshot(&context.thread_id).await,
            ActionKind::CurrentThreadHistory => {
                self.thread_history(&context.thread_id, arguments).await
            }
            ActionKind::TurnStart => self.start(&context.thread_id, arguments).await,
            ActionKind::TurnSteer => {
                self.steer(&context.thread_id, &context.turn_id, arguments)
                    .await
            }
            ActionKind::TurnInterrupt => self.interrupt(&context.thread_id, &context.turn_id).await,
            ActionKind::ThreadList => {
                let threads = self
                    .handle
                    .list_threads()
                    .await
                    .map_err(|error| ActionError::Protocol(error.to_string()))?;
                let threads = threads
                    .into_iter()
                    .map(|thread| (thread.id.clone(), thread))
                    .collect::<BTreeMap<_, _>>();
                serde_json::to_value(threads)
                    .map_err(|error| ActionError::Protocol(error.to_string()))
            }
            ActionKind::ArbitraryThreadSnapshot => {
                self.thread_snapshot(string_param(arguments, "thread_id")?)
                    .await
            }
            ActionKind::ArbitraryThreadHistory => {
                self.thread_history(string_param(arguments, "thread_id")?, arguments)
                    .await
            }
            ActionKind::ArbitraryTurnStart => {
                self.start(string_param(arguments, "thread_id")?, arguments)
                    .await
            }
            ActionKind::ArbitraryTurnSteer => {
                self.steer(
                    string_param(arguments, "thread_id")?,
                    string_param(arguments, "turn_id")?,
                    arguments,
                )
                .await
            }
            ActionKind::ArbitraryTurnInterrupt => {
                self.interrupt(
                    string_param(arguments, "thread_id")?,
                    string_param(arguments, "turn_id")?,
                )
                .await
            }
        }
    }

    async fn thread_snapshot(&self, thread_id: &str) -> Result<Value, ActionError> {
        let snapshot = self.handle.snapshot().await;
        Ok(json!({
            "at_sequence": snapshot.at_sequence,
            "thread": snapshot.threads.get(thread_id),
        }))
    }

    async fn thread_history(
        &self,
        thread_id: &str,
        arguments: &Value,
    ) -> Result<Value, ActionError> {
        let after = arguments.get("after").and_then(Value::as_u64).unwrap_or(0);
        let through = arguments.get("through").and_then(Value::as_u64);
        let result = self
            .handle
            .query_sequence(Some(thread_id), after, through)
            .await;
        Ok(json!({
            "events": result.events.iter().map(|event| event.frame.raw()).collect::<Vec<_>>(),
            "gap": result.gap,
        }))
    }

    async fn start(&self, thread_id: &str, arguments: &Value) -> Result<Value, ActionError> {
        let input = input_param(arguments)?;
        encode_outcome(self.handle.start(thread_id.to_owned(), input).await)
    }

    async fn steer(
        &self,
        thread_id: &str,
        turn_id: &str,
        arguments: &Value,
    ) -> Result<Value, ActionError> {
        let input = input_param(arguments)?;
        encode_outcome(
            self.handle
                .steer(thread_id.to_owned(), turn_id.to_owned(), input)
                .await,
        )
    }

    async fn interrupt(&self, thread_id: &str, turn_id: &str) -> Result<Value, ActionError> {
        encode_outcome(
            self.handle
                .interrupt(thread_id.to_owned(), turn_id.to_owned())
                .await,
        )
    }

    pub async fn serve(
        &self,
        mut shutdown: watch::Receiver<bool>,
        max_bytes: usize,
    ) -> io::Result<()> {
        validate_message_limit(max_bytes)?;
        let _socket_lock = prepare_socket(&self.socket_path)?;
        let listener = UnixListener::bind(&self.socket_path)?;
        set_private_permissions(&self.socket_path)?;
        let mut clients = JoinSet::new();
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let gateway = self.clone();
                    clients.spawn(async move {
                        if let Err(error) = gateway.handle_stream(stream, max_bytes).await {
                            tracing::warn!(%error, "action gateway client failed");
                        }
                    });
                }
                joined = clients.join_next(), if !clients.is_empty() => {
                    if let Some(Err(error)) = joined {
                        tracing::warn!(%error, "action gateway client task failed");
                    }
                }
            }
        }
        clients.abort_all();
        while clients.join_next().await.is_some() {}
        self.shutdown_non_blocking().await;
        let _ = std::fs::remove_file(&self.socket_path);
        Ok(())
    }

    async fn handle_stream(&self, stream: UnixStream, max_bytes: usize) -> Result<(), ActionError> {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut bytes = Vec::new();
        let count = {
            let mut bounded = (&mut reader).take(u64::try_from(max_bytes).unwrap_or(u64::MAX) + 1);
            bounded.read_until(b'\n', &mut bytes).await?
        };
        if count == 0 || count > max_bytes || !bytes.ends_with(b"\n") {
            return Err(ActionError::Protocol(
                "request is empty, unterminated, or exceeds the message limit".into(),
            ));
        }
        let request: GatewayRequest = serde_json::from_slice(&bytes)
            .map_err(|error| ActionError::Protocol(error.to_string()))?;
        if !valid_request_id(&request.id) {
            let response = GatewayResponse {
                message_type: "response".into(),
                protocol_version: ACTION_PROTOCOL_VERSION,
                id: String::new(),
                ok: false,
                result: None,
                error: Some(GatewayErrorBody {
                    code: "protocol_error".into(),
                    message: "request id must be 1-128 safe ASCII bytes".into(),
                }),
            };
            writer
                .write_all(&encode_response(response, max_bytes)?)
                .await?;
            writer.shutdown().await?;
            return Ok(());
        }
        let native_hook_event = request.method == "warden.native_hook.event";
        let response = if native_hook_event {
            // A hook may intentionally interrupt its own source turn and then perform
            // follow-up actions. Codex tears down that turn's bridge process as soon as
            // interruption succeeds, so the native request must outlive its client socket.
            // Invocation timeouts still provide the hard execution bound.
            self.dispatch(request).await
        } else {
            let dispatch = self.dispatch(request);
            tokio::pin!(dispatch);
            let disconnected = wait_for_full_disconnect(&mut reader, &writer);
            tokio::pin!(disconnected);
            tokio::select! {
                response = &mut dispatch => response,
                disconnected = &mut disconnected => {
                    disconnected?;
                    return Ok(());
                }
            }
        };
        let encoded = encode_response(response, max_bytes)?;
        if let Err(error) = writer.write_all(&encoded).await {
            if native_hook_event && is_disconnect_error(&error) {
                return Ok(());
            }
            return Err(ActionError::Io(error));
        }
        if let Err(error) = writer.shutdown().await {
            if native_hook_event && is_disconnect_error(&error) {
                return Ok(());
            }
            return Err(ActionError::Io(error));
        }
        Ok(())
    }
}

async fn wait_for_full_disconnect(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &tokio::net::unix::OwnedWriteHalf,
) -> Result<(), ActionError> {
    let mut unexpected = [0_u8; 1];
    if reader.read(&mut unexpected).await? != 0 {
        return Err(ActionError::Protocol(
            "one request is allowed per connection".into(),
        ));
    }

    // EOF only proves that the peer closed its write half. A client may do that
    // after sending its request while keeping its read half open for the reply.
    // A zero-byte socket write reaches the kernel without adding protocol data:
    // it succeeds for a half-closed peer and fails once the peer is fully gone.
    loop {
        match writer.try_write(&[]) {
            Err(error) if is_disconnect_error(&error) => {
                return Ok(());
            }
            Ok(_) => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(ActionError::Io(error)),
        }
    }
}

fn is_disconnect_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
    )
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn response_too_large(id: String) -> GatewayResponse {
    GatewayResponse {
        message_type: "response".into(),
        protocol_version: ACTION_PROTOCOL_VERSION,
        id,
        ok: false,
        result: None,
        error: Some(GatewayErrorBody {
            code: "response_too_large".into(),
            message: "Warden response exceeds the configured message limit".into(),
        }),
    }
}

fn validate_message_limit(max_bytes: usize) -> io::Result<()> {
    let largest_bounded_error =
        serde_json::to_vec(&response_too_large("x".repeat(MAX_REQUEST_ID_BYTES)))
            .map_err(io::Error::other)?
            .len()
            .saturating_add(1);
    if max_bytes < largest_bounded_error {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("action message limit must be at least {largest_bounded_error} bytes"),
        ));
    }
    Ok(())
}

fn encode_response(response: GatewayResponse, max_bytes: usize) -> Result<Vec<u8>, ActionError> {
    if let Some(encoded) = serialize_response_bounded(&response, max_bytes)? {
        return Ok(encoded);
    }
    serialize_response_bounded(&response_too_large(response.id), max_bytes)?.ok_or_else(|| {
        ActionError::Protocol("bounded error response exceeds the message limit".into())
    })
}

fn serialize_response_bounded(
    response: &GatewayResponse,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, ActionError> {
    let mut output = BoundedJsonBuffer::new(max_bytes.saturating_sub(1));
    match serde_json::to_writer(&mut output, response) {
        Ok(()) => {
            output.bytes.push(b'\n');
            Ok(Some(output.bytes))
        }
        Err(_) if output.exceeded => Ok(None),
        Err(error) => Err(ActionError::Protocol(error.to_string())),
    }
}

struct BoundedJsonBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl BoundedJsonBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(8 * 1024)),
            limit,
            exceeded: false,
        }
    }
}

impl io::Write for BoundedJsonBuffer {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "serialized response exceeds message limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn string_param<'a>(value: &'a Value, name: &'static str) -> Result<&'a str, ActionError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or(ActionError::InvalidParameter(name))
}

fn optional_string_param(value: &Value, name: &'static str) -> Result<Option<String>, ActionError> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(ActionError::InvalidParameter(name)),
    }
}

fn optional_non_empty_string_param(
    value: &Value,
    name: &'static str,
) -> Result<Option<String>, ActionError> {
    let Some(value) = optional_string_param(value, name)? else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(ActionError::InvalidParameter(name));
    }
    Ok(Some(value.to_owned()))
}

fn validate_agent_inference_params(
    context: &InvocationContext,
    params: &Value,
) -> Result<(), ActionError> {
    let expected_event = serde_json::to_value(&context.event)
        .map_err(|error| ActionError::Protocol(error.to_string()))?;
    if params.get("event") != Some(&expected_event) {
        return Err(ActionError::InvalidParameter("event"));
    }
    match params.get("options") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(options)) if options.is_empty() => Ok(()),
        _ => Err(ActionError::InvalidParameter("options")),
    }
}

async fn await_agent_call<F>(context: &InvocationContext, future: F) -> Result<Value, ActionError>
where
    F: Future<Output = Result<Value, String>>,
{
    let mut cancellation = context.cancellation.subscribe();
    if *cancellation.borrow() {
        return Err(ActionError::InvocationCancelled);
    }
    tokio::select! {
        result = future => result.map_err(ActionError::Agent),
        changed = cancellation.changed() => {
            let _ = changed;
            Err(ActionError::InvocationCancelled)
        }
    }
}

fn input_param(value: &Value) -> Result<Vec<Value>, ActionError> {
    value
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .ok_or(ActionError::InvalidParameter("input"))
}

fn encode_outcome(outcome: ActionOutcome) -> Result<Value, ActionError> {
    serde_json::to_value(outcome).map_err(|error| ActionError::Protocol(error.to_string()))
}

fn error_code(error: &ActionError) -> &'static str {
    match error {
        ActionError::UnknownAction(_) => "unknown_action",
        ActionError::MissingCredential | ActionError::InvalidCredential => "unauthorized",
        ActionError::AccessDenied(_) | ActionError::TargetScope { .. } => "access_denied",
        ActionError::InvalidParameter(_) => "invalid_parameter",
        ActionError::Agent(_) => "agent_failure",
        ActionError::InvocationCancelled => "invocation_cancelled",
        ActionError::Protocol(_) => "protocol_error",
        ActionError::Io(_) => "io_error",
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(a ^ b);
    }
    difference == 0
}

fn prepare_socket(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::FileTypeExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut lock_name = path.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(PathBuf::from(lock_name))?;
    FileExt::try_lock_exclusive(&lock).map_err(|error| {
        io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another Warden daemon owns the action socket: {error}"),
        )
    })?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "the action socket already has a live listener",
                    ));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(path)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "action socket path is occupied by a non-socket file",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(lock)
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_and_scope_are_explicit() {
        assert!(!ActionKind::TurnInterrupt.is_cross_thread());
        assert!(ActionKind::ThreadList.is_cross_thread());
        assert!(ActionKind::ArbitraryTurnInterrupt.is_cross_thread());
        assert_eq!(
            ActionKind::parse("current_event").unwrap(),
            ActionKind::CurrentEvent
        );
        assert!(matches!(
            ActionKind::parse("shell"),
            Err(ActionError::UnknownAction(_))
        ));
    }

    #[test]
    fn credential_comparison_handles_different_lengths() {
        assert!(constant_time_equal(b"secret", b"secret"));
        assert!(!constant_time_equal(b"secret", b"secrex"));
        assert!(!constant_time_equal(b"secret", b"secret-long"));
    }

    #[test]
    fn socket_lock_refuses_a_second_daemon_and_allows_restart() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let socket = directory.path().join("warden.sock");
        let first_lock = prepare_socket(&socket).expect("first daemon lock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");
        let second = prepare_socket(&socket).expect_err("second daemon must be rejected");
        assert_eq!(second.kind(), io::ErrorKind::AddrInUse);
        drop(listener);
        drop(first_lock);
        assert!(socket.exists(), "simulate a socket left behind by a crash");
        let _restart_lock = prepare_socket(&socket).expect("restart acquires released lock");
        assert!(
            !socket.exists(),
            "restart removes the stale socket before bind"
        );
    }
}
