use crate::{
    action::{ActionGateway, AgentBackend, AgentCallContext},
    activation::ActivationRouter,
    config::Config,
    event::{normalize_event_with_input, user_message_content},
    native_hook::{BRIDGE_EVENTS, NativeHookInstall},
    onboarding::reconcile_codex,
    python::PythonRuntime,
    registry::HookRegistry,
};
use async_trait::async_trait;
use codex_control::{
    CodexControl, ConnectionPhase, Handle, HookTrustUpdate, IncomingFrame, LifecycleItem,
    ListedThread, SubscriptionState, init_tracing,
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, File, OpenOptions},
    future::Future,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{RwLock, watch};
use warden_agent::{
    AgentInput, AgentSessions, ClaudeCliDriver, CliConfig, CodexCliDriver, InvocationEnvironment,
    ProviderDriver, ProviderKind, SessionKey, SessionSnapshot,
};

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Control(#[from] codex_control::Error),
    #[error(transparent)]
    Registry(#[from] crate::registry::RegistryError),
    #[error(transparent)]
    Skill(#[from] codex_control::SkillManagementError),
    #[error(transparent)]
    ThreadList(#[from] codex_control::ThreadListError),
    #[error("runtime I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("hook watcher failed: {0}")]
    Watch(#[from] notify::Error),
}

pub struct Warden;

impl Warden {
    pub async fn run(config: Config) -> Result<(), RuntimeError> {
        init_tracing("warden_daemon=info,warn");
        let onboarding = reconcile_codex(&config).map_err(|source| RuntimeError::Io {
            path: config.codex_home.clone(),
            source,
        })?;
        tracing::info!(
            authoring_skill = %onboarding.authoring_skill.display(),
            skill_changed = onboarding.skill_changed,
            hook_templates = ?onboarding.hook_templates,
            native_hooks = %onboarding.native_hooks.hooks_file.display(),
            bridge_changed = onboarding.native_hooks.changed,
            restart_required = onboarding.native_hooks.changed,
            manage_gui = config.manage_gui,
            "Codex onboarding reconciled"
        );
        let control_config = control_config(config.manage_gui);
        CodexControl::run(control_config, move |handle| async move {
            Self::serve_until_with_native_hook(config, handle, onboarding.native_hooks, async {
                if let Err(error) = tokio::signal::ctrl_c().await {
                    tracing::error!(%error, "failed to install Ctrl-C handler");
                }
            })
            .await
        })
        .await?
    }

    pub async fn serve_until<F>(
        config: Config,
        handle: Handle,
        shutdown_signal: F,
    ) -> Result<(), RuntimeError>
    where
        F: Future<Output = ()> + Send,
    {
        let onboarding = reconcile_codex(&config).map_err(|source| RuntimeError::Io {
            path: config.codex_home.clone(),
            source,
        })?;
        Self::serve_until_with_native_hook(config, handle, onboarding.native_hooks, shutdown_signal)
            .await
    }

    async fn serve_until_with_native_hook<F>(
        config: Config,
        handle: Handle,
        native_hook: NativeHookInstall,
        shutdown_signal: F,
    ) -> Result<(), RuntimeError>
    where
        F: Future<Output = ()> + Send,
    {
        config
            .paths
            .create_all()
            .map_err(|source| RuntimeError::Io {
                path: config.paths.root.clone(),
                source,
            })?;

        let python = Arc::new(PythonRuntime::new(&config));
        let registry = HookRegistry::new(
            config.paths.hooks.clone(),
            config.paths.modules.clone(),
            config.paths.generated_skills.clone(),
            config.paths.runtimes.clone(),
            python.clone(),
        );
        let initial_delta = registry.refresh().await?;
        log_registry_delta(&initial_delta);

        let skill_root = canonical(&config.paths.generated_skills)?;
        handle.set_skill_extra_roots([skill_root.clone()]).await?;
        let skill_targets = SkillRefreshTargets::from_handle(&handle, &config.paths.root).await?;
        let _ = handle
            .force_refresh_skills(skill_targets.all().await)
            .await?;

        let bridge_trusted =
            trust_native_bridge_bundle(&handle, &native_hook, &config.paths.root).await;

        let agent_sessions = Arc::new(
            AgentSessions::new(agent_drivers(&config))
                .expect("the built-in provider list has unique provider identities"),
        );
        let agents = Arc::new(ManagedAgents::new(
            agent_sessions.clone(),
            config.paths.sessions.clone(),
        ));
        let router = ActivationRouter::new(config.paths.generated_skills.clone(), registry.clone());
        let bridge_token = fs::read_to_string(&native_hook.credential_file)
            .map_err(|source| RuntimeError::Io {
                path: native_hook.credential_file.clone(),
                source,
            })?
            .trim()
            .to_owned();
        let gateway =
            ActionGateway::new(handle.clone(), config.paths.action_socket.clone(), agents)
                .with_native_hook_runtime(
                    router.clone(),
                    python.clone(),
                    bridge_token,
                    config.max_concurrent_hooks,
                );
        let metrics = RuntimeMetrics::default();
        gateway
            .merge_daemon_health(json!({
                "bridge": {
                    "configured": true,
                    "trusted": bridge_trusted,
                    "loaded_confirmed": false,
                    "loaded_tasks": [],
                    "restart_required": native_hook.changed,
                    "events": BRIDGE_EVENTS,
                }
            }))
            .await;
        update_daemon_health(&gateway, &registry, metrics.snapshot()).await;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let action_gateway = gateway.clone();
        let max_message_bytes = config.max_hook_message_bytes;
        let action_task =
            tokio::spawn(async move { action_gateway.serve(shutdown_rx, max_message_bytes).await });

        let event_handle = handle.clone();
        let event_router = router.clone();
        let event_python = python.clone();
        let event_gateway = gateway.clone();
        let event_registry = registry.clone();
        let event_skill_targets = skill_targets.clone();
        let event_metrics = metrics.clone();
        let event_task = tokio::spawn(async move {
            process_events(
                event_handle,
                event_router,
                event_python,
                event_gateway,
                event_registry,
                event_skill_targets,
                event_metrics,
            )
            .await;
        });
        let cwd_delta_handle = handle.clone();
        let cwd_delta_targets = skill_targets.clone();
        let cwd_delta_task = tokio::spawn(async move {
            process_skill_cwd_deltas(cwd_delta_handle, cwd_delta_targets).await;
        });

        let (watch_tx, mut watch_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = create_watcher(watch_tx)?;
        watcher.watch(&config.paths.hooks, RecursiveMode::Recursive)?;
        watcher.watch(&config.paths.modules, RecursiveMode::Recursive)?;
        watcher.watch(&config.paths.generated_skills, RecursiveMode::Recursive)?;

        let mut transport_health = handle.health();
        let refresh_handle = handle.clone();
        let refresh_registry = registry.clone();
        let refresh_gateway = gateway.clone();
        let refresh_skill_targets = skill_targets.clone();
        let refresh_metrics = metrics.clone();
        let mut previous_phase = transport_health.borrow().phase;
        tokio::pin!(shutdown_signal);
        loop {
            tokio::select! {
                () = &mut shutdown_signal => break,
                changed = transport_health.changed() => {
                    if changed.is_err() { break; }
                    let phase = transport_health.borrow().phase;
                    if phase == ConnectionPhase::Connected && previous_phase != ConnectionPhase::Connected {
                        // The dependency has already replayed the root before opening its gate.
                        if let Err(error) = refresh_skill_targets.refresh_from_handle(&refresh_handle).await {
                            tracing::error!(%error, "could not refresh task CWDs after app-server reconnect");
                        }
                        if let Err(error) = refresh_handle.force_refresh_skills(refresh_skill_targets.all().await).await {
                            tracing::error!(%error, "skill refresh after app-server reconnect failed");
                        }
                    }
                    previous_phase = phase;
                }
                event = watch_rx.recv() => {
                    let Some(event) = event else { break; };
                    let mut should_refresh = watch_event_requires_refresh(event);
                    // One source write commonly arrives as create, data, metadata, and rename
                    // notifications. Coalesce that burst into one candidate transaction.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    while let Ok(event) = watch_rx.try_recv() {
                        should_refresh |= watch_event_requires_refresh(event);
                    }
                    if should_refresh {
                        match refresh_registry.refresh().await {
                            Ok(delta) => {
                                log_registry_delta(&delta);
                                if let Err(error) = refresh_skill_targets.refresh_from_handle(&refresh_handle).await {
                                    tracing::error!(%error, "could not refresh task CWDs after hook change");
                                }
                                if let Err(error) = refresh_handle.force_refresh_skills(refresh_skill_targets.all().await).await {
                                    tracing::error!(%error, "skill refresh after hook change failed");
                                }
                                update_daemon_health(&refresh_gateway, &refresh_registry, refresh_metrics.snapshot()).await;
                            }
                            Err(error) => tracing::error!(%error, "hook registry refresh failed"),
                        }
                    }
                }
            }
        }

        let _ = shutdown_tx.send(true);
        event_task.abort();
        let _ = event_task.await;
        cwd_delta_task.abort();
        let _ = cwd_delta_task.await;
        python.shutdown().await;
        if let Err(error) = agent_sessions.shutdown().await {
            tracing::warn!(%error, "agent shutdown reported an error");
        }
        match action_task.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "action gateway stopped with an error"),
            Err(error) if !error.is_cancelled() => {
                tracing::warn!(%error, "action gateway task failed")
            }
            Err(_) => {}
        }
        drop(watcher);
        Ok(())
    }
}

fn control_config(manage_gui: bool) -> codex_control::Config {
    let mut config = codex_control::Config {
        manage_gui,
        ..codex_control::Config::default()
    };
    // Warden is event-driven. Historical reconciliation would read up to hundreds of
    // unrelated retained tasks merely to discover whether any are active.
    config.ingest.reconcile_candidate_limit = 0;
    config
}

async fn trust_native_bridge_bundle(
    handle: &Handle,
    install: &NativeHookInstall,
    query_cwd: &Path,
) -> bool {
    let listed = match handle.list_hooks([query_cwd.to_path_buf()]).await {
        Ok(listed) => listed,
        Err(error) => {
            tracing::warn!(%error, "could not inspect Codex native hook trust for Warden bridge");
            return false;
        }
    };
    let expected_source =
        canonical(&install.hooks_file).unwrap_or_else(|_| install.hooks_file.clone());
    let mut discovered = BTreeSet::new();
    let mut trusted = BTreeSet::new();
    let updates = listed
        .data
        .into_iter()
        .flat_map(|entry| entry.hooks)
        .filter(|hook| {
            let source = canonical(&hook.source_path).unwrap_or_else(|_| hook.source_path.clone());
            let owned = BRIDGE_EVENTS
                .iter()
                .any(|event| hook.event_name.eq_ignore_ascii_case(event))
                && hook.command.as_deref() == Some(install.command.as_str())
                && source == expected_source
                && hook.enabled;
            if owned {
                discovered.insert(hook.event_name.to_ascii_lowercase());
                if matches!(hook.trust_status.as_str(), "trusted" | "managed") {
                    trusted.insert(hook.event_name.to_ascii_lowercase());
                }
            }
            owned && !matches!(hook.trust_status.as_str(), "trusted" | "managed")
        })
        .map(|hook| HookTrustUpdate {
            key: hook.key,
            current_hash: hook.current_hash,
        })
        .collect::<Vec<_>>();
    if discovered.len() != BRIDGE_EVENTS.len() {
        tracing::warn!(
            expected = BRIDGE_EVENTS.len(),
            discovered = discovered.len(),
            "Codex did not report the complete Warden native bridge bundle"
        );
        return false;
    }
    if updates.is_empty() {
        return trusted.len() == BRIDGE_EVENTS.len();
    }
    match handle.trust_hooks(updates).await {
        Ok(()) => {
            tracing::info!("trusted Warden native bridge bundle through Codex config API");
            true
        }
        Err(error) => {
            tracing::warn!(%error, "could not trust Warden native bridge bundle");
            false
        }
    }
}

async fn process_events(
    handle: Handle,
    router: ActivationRouter,
    python: Arc<PythonRuntime>,
    gateway: ActionGateway,
    registry: HookRegistry,
    skill_targets: SkillRefreshTargets,
    metrics: RuntimeMetrics,
) {
    let mut lifecycle = handle.lifecycle(0);
    let mut last_sequence = 0;
    loop {
        match lifecycle.recv().await {
            LifecycleItem::Event(source) => {
                last_sequence = last_sequence.max(source.sequence);
                refresh_for_new_task_cwd(&handle, &skill_targets, &source).await;
                recover_active_prompt(&handle, &router, &python, &gateway, &source).await;
                process_source(&router, &python, &gateway, source).await;
                metrics.record(last_sequence, router.gaps().await.len());
                update_daemon_health(&gateway, &registry, metrics.snapshot()).await;
            }
            LifecycleItem::Replay(events) => {
                for source in events {
                    last_sequence = last_sequence.max(source.sequence);
                    refresh_for_new_task_cwd(&handle, &skill_targets, &source).await;
                    recover_active_prompt(&handle, &router, &python, &gateway, &source).await;
                    process_source(&router, &python, &gateway, source).await;
                }
                metrics.record(last_sequence, router.gaps().await.len());
                update_daemon_health(&gateway, &registry, metrics.snapshot()).await;
            }
            LifecycleItem::GapTooOld {
                snapshot,
                oldest_available,
            } => {
                let gap = router
                    .note_gap(last_sequence, oldest_available, snapshot.at_sequence)
                    .await;
                tracing::error!(
                    after_sequence = gap.after_sequence,
                    oldest_available = ?gap.oldest_available,
                    snapshot_sequence = gap.snapshot_sequence,
                    expired_activations = gap.expired_activations,
                    "lifecycle coverage gap; activations conservatively expired"
                );
                last_sequence = snapshot.at_sequence;
                let raw_threads = snapshot
                    .threads
                    .values()
                    .filter_map(|thread| thread.raw_thread.clone())
                    .collect::<Vec<_>>();
                let discovered = skill_targets.observe_snapshot_threads(&raw_threads).await;
                if discovered
                    && let Err(error) = handle.force_refresh_skills(skill_targets.all().await).await
                {
                    tracing::error!(%error, "skill refresh for task CWD recovered from snapshot failed");
                }
                metrics.record(last_sequence, router.gaps().await.len());
                update_daemon_health(&gateway, &registry, metrics.snapshot()).await;
            }
            LifecycleItem::Closed => break,
        }
    }
}

async fn process_skill_cwd_deltas(handle: Handle, targets: SkillRefreshTargets) {
    let mut deltas = handle.deltas();
    while let Some(source) = deltas.recv().await {
        refresh_for_new_task_cwd(&handle, &targets, &source).await;
    }
}

async fn process_source(
    router: &ActivationRouter,
    python: &Arc<PythonRuntime>,
    gateway: &ActionGateway,
    source: Arc<codex_control::SequencedEvent>,
) {
    process_source_with_input(router, python, gateway, source, None).await;
}

async fn process_source_with_input(
    router: &ActivationRouter,
    python: &Arc<PythonRuntime>,
    gateway: &ActionGateway,
    source: Arc<codex_control::SequencedEvent>,
    retained_input: Option<Value>,
) {
    let activated = router
        .begin_from_source_with_input(&source, retained_input.as_ref())
        .await;
    if !activated.is_empty() {
        tracing::info!(sequence = source.sequence, hooks = ?activated, "activated Warden hooks for turn");
    }
    for event in normalize_event_with_input(source, retained_input) {
        let deliveries = router.route(event).await;
        gateway
            .dispatch_deliveries(python.clone(), deliveries)
            .await;
    }
}

async fn recover_active_prompt(
    handle: &Handle,
    router: &ActivationRouter,
    python: &Arc<PythonRuntime>,
    gateway: &ActionGateway,
    status_source: &Arc<codex_control::SequencedEvent>,
) {
    if !is_active_status_signal(status_source) {
        return;
    }
    let Some(thread_id) = status_source.thread_id.as_deref() else {
        return;
    };

    let _ = handle.observe_thread(thread_id).await;
    let subscribed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match handle.subscription_states().await.get(thread_id) {
                Some(SubscriptionState::Subscribed) => break true,
                Some(
                    SubscriptionState::Ephemeral
                    | SubscriptionState::CapacityDegraded
                    | SubscriptionState::Released
                    | SubscriptionState::Failed(_),
                ) => break false,
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap_or(false);
    if !subscribed {
        tracing::warn!(
            thread_id,
            "could not recover active task input: task was not subscribed"
        );
        return;
    }

    let thread = match handle.read_thread(thread_id).await {
        Ok(thread) => thread,
        Err(error) => {
            tracing::warn!(thread_id, %error, "could not recover active task input");
            return;
        }
    };
    let Some((turn_id, input)) = thread.turns.iter().rev().find_map(|turn| {
        if turn_is_terminal(turn.metadata.get("status")) {
            return None;
        }
        user_message_content(&turn.items).map(|input| (turn.id.as_str(), input))
    }) else {
        return;
    };
    let raw = json!({
        "jsonrpc": "2.0",
        "method": "turn/started",
        "params": {"threadId": thread_id, "turn": {"id": turn_id}},
    });
    let Ok(frame) = IncomingFrame::parse(raw) else {
        return;
    };
    let mut recovered = codex_control::SequencedEvent::from_frame(
        status_source.sequence,
        Duration::from_millis(status_source.monotonic_ms),
        frame,
    );
    recovered.reconstructed = true;
    process_source_with_input(router, python, gateway, Arc::new(recovered), Some(input)).await;
}

fn is_active_status_signal(source: &codex_control::SequencedEvent) -> bool {
    source.method() == Some("thread/status/changed")
        && source
            .frame
            .params()
            .and_then(|params| params.get("status"))
            .and_then(|status| status.as_str().or_else(|| status.get("type")?.as_str()))
            == Some("active")
}

fn turn_is_terminal(status: Option<&Value>) -> bool {
    status
        .and_then(|status| status.as_str().or_else(|| status.get("type")?.as_str()))
        .is_some_and(|status| {
            matches!(
                status,
                "completed" | "failed" | "interrupted" | "cancelled" | "canceled"
            )
        })
}

fn agent_drivers(config: &Config) -> Vec<Arc<dyn ProviderDriver>> {
    let limits = config.max_hook_message_bytes;
    let claude = CliConfig::new("claude")
        .with_current_dir(config.paths.root.clone())
        .with_timeout(config.agent_timeout)
        .with_limits(limits, limits.saturating_mul(8), limits.saturating_mul(2));
    let codex = CliConfig::new("codex")
        .with_current_dir(config.paths.root.clone())
        .with_timeout(config.agent_timeout)
        .with_limits(limits, limits.saturating_mul(8), limits.saturating_mul(2));
    vec![
        Arc::new(
            ClaudeCliDriver::new(claude)
                .with_extra_arg("--allowedTools")
                .with_extra_arg("Bash(warden *)")
                .with_extra_arg("--permission-mode")
                .with_extra_arg("dontAsk"),
        ),
        Arc::new(CodexCliDriver::new(codex)),
    ]
}

struct ManagedAgents {
    sessions: Arc<AgentSessions>,
    sessions_root: PathBuf,
    restore_lock: tokio::sync::Mutex<()>,
    operation_locks: tokio::sync::Mutex<HashMap<SessionKey, Arc<tokio::sync::Mutex<()>>>>,
    source_epoch: uuid::Uuid,
    next_agent_sequence: AtomicU64,
}

impl ManagedAgents {
    fn new(sessions: Arc<AgentSessions>, sessions_root: PathBuf) -> Self {
        Self {
            sessions,
            sessions_root,
            restore_lock: tokio::sync::Mutex::new(()),
            operation_locks: tokio::sync::Mutex::new(HashMap::new()),
            source_epoch: uuid::Uuid::new_v4(),
            next_agent_sequence: AtomicU64::new(1),
        }
    }

    fn provider(value: &str) -> Result<ProviderKind, String> {
        match value {
            "claude" => Ok(ProviderKind::Claude),
            "codex" => Ok(ProviderKind::Codex),
            _ => Err(format!("unsupported agent provider {value:?}")),
        }
    }

    fn model(provider: ProviderKind, model: Option<String>) -> Result<Option<String>, String> {
        if model.is_some() && provider != ProviderKind::Claude {
            return Err(format!(
                "explicit model selection is not supported for {provider} agent hooks"
            ));
        }
        Ok(model)
    }

    fn key(context: &AgentCallContext, provider: ProviderKind, name: &str) -> SessionKey {
        SessionKey::new(
            provider,
            context.hook_id.as_str(),
            name,
            &context.source_thread_id,
        )
    }

    fn input(
        &self,
        context: &AgentCallContext,
        prompt: Option<String>,
    ) -> Result<AgentInput, String> {
        let event = serde_json::to_value(&context.event).map_err(|error| error.to_string())?;
        let mut guidance = prompt.unwrap_or_default();
        let mut actions = context
            .grant
            .actions
            .iter()
            .map(|action| {
                serde_json::to_value(action)
                    .expect("action enum serializes")
                    .as_str()
                    .expect("action serializes as string")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        actions.sort();
        if actions.is_empty() {
            guidance.push_str("\n\nThis invocation grants no Warden control actions.");
        } else {
            guidance.push_str(&format!(
                "\n\nGranted Warden actions: {}. Credentials and the `warden` CLI are already injected. Use `warden action <name> --arguments '<json>'` only for a listed action.",
                actions.join(", ")
            ));
        }
        // Native barriers and observed app-server events have independent sequence spaces.
        // Persistent sends are already serialized by their SessionKey operation lock, so give
        // agent calls one daemon-local cursor instead of comparing unrelated upstream numbers.
        let agent_sequence = self.next_agent_sequence.fetch_add(1, Ordering::AcqRel);
        Ok(AgentInput::new(agent_sequence, event)
            .with_source_epoch(self.source_epoch)
            .with_prompt(guidance))
    }

    fn environment(context: &AgentCallContext) -> InvocationEnvironment {
        let cli = warden_cli_path();
        let mut environment = InvocationEnvironment::new()
            .with_var("WARDEN_SOCKET", context.credential.socket.as_os_str())
            .with_var(
                "WARDEN_INVOCATION_ID",
                context.credential.invocation_id.to_string(),
            )
            // Codex deliberately filters variables whose names look like secrets or tokens
            // before model-run shell commands. This narrowly scoped name remains available
            // to the injected `warden` client without enabling broad secret inheritance.
            .with_var("WARDEN_INVOCATION_AUTH", &context.credential.token)
            .with_var("WARDEN_CLI", cli.as_os_str());
        if let Some(parent) = cli.parent() {
            let mut paths = vec![parent.to_owned()];
            if let Some(existing) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            if let Ok(path) = std::env::join_paths(paths) {
                environment.insert("PATH", path);
            }
        }
        environment
    }

    fn record_path(&self, key: &SessionKey) -> PathBuf {
        let bytes = serde_json::to_vec(key).expect("session key serializes");
        let hash = hex::encode(Sha256::digest(bytes));
        self.sessions_root
            .join(key.provider.to_string())
            .join(format!("{hash}.json"))
    }

    fn pending_path(&self, key: &SessionKey) -> PathBuf {
        self.record_path(key).with_extension("pending.json")
    }

    async fn operation_lock(&self, key: &SessionKey) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.operation_locks.lock().await;
        Arc::clone(
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    async fn ensure_ready_and_restore(&self, key: &SessionKey) -> Result<(), String> {
        if let Some(reason) = self.pending_reason(key)? {
            self.sessions
                .mark_session_unavailable(key.clone(), reason.clone())
                .await
                .map_err(|error| error.to_string())?;
            return Err(unavailable_message(key, &reason));
        }
        match self.sessions.session_status(key).await {
            Ok(Some(_)) => return Ok(()),
            Err(error) => return Err(error.to_string()),
            Ok(None) => {}
        }
        let _guard = self.restore_lock.lock().await;
        match self.sessions.session_status(key).await {
            Ok(Some(_)) => return Ok(()),
            Err(error) => return Err(error.to_string()),
            Ok(None) => {}
        }
        let path = self.record_path(key);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        let record: DurableSession = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid session record {}: {error}", path.display()))?;
        if &record.key != key {
            return Err(format!(
                "session record {} has the wrong key",
                path.display()
            ));
        }
        self.sessions
            .restore_session(key.clone(), record.snapshot)
            .await
            .map_err(|error| error.to_string())
    }

    fn pending_reason(&self, key: &SessionKey) -> Result<Option<String>, String> {
        let path = self.pending_path(key);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        let detail = match serde_json::from_slice::<DurablePending>(&bytes) {
            Ok(pending) if pending.key == *key => format!(
                "durable transaction for source epoch {} sequence {} ordinal {} did not commit",
                pending
                    .source_epoch
                    .map_or_else(|| "unknown".into(), |epoch| epoch.to_string()),
                pending.source_sequence,
                pending.source_ordinal
            ),
            Ok(_) => format!(
                "pending marker {} has the wrong session key",
                path.display()
            ),
            Err(error) => format!("pending marker {} is invalid: {error}", path.display()),
        };
        Ok(Some(detail))
    }

    fn begin_pending(&self, key: &SessionKey, input: &AgentInput) -> Result<(), String> {
        if let Some(reason) = self.pending_reason(key)? {
            return Err(unavailable_message(key, &reason));
        }
        atomic_write_json(
            &self.pending_path(key),
            &DurablePending {
                key: key.clone(),
                source_epoch: input.source_epoch,
                source_sequence: input.source_sequence,
                source_ordinal: input.source_ordinal,
            },
        )
    }

    fn commit_snapshot(&self, key: &SessionKey, snapshot: SessionSnapshot) -> Result<(), String> {
        let path = self.record_path(key);
        atomic_write_json(
            &path,
            &DurableSession {
                key: key.clone(),
                snapshot,
            },
        )?;

        let pending = self.pending_path(key);
        fs::remove_file(&pending)
            .map_err(|error| format!("could not clear {}: {error}", pending.display()))?;
        sync_parent(&pending)
    }

    fn remove_artifacts(&self, key: &SessionKey) -> Result<bool, String> {
        let mut removed = false;
        let mut parent = None;
        for path in [self.record_path(key), self.pending_path(key)] {
            match fs::remove_file(&path) {
                Ok(()) => {
                    removed = true;
                    parent = path.parent().map(Path::to_owned);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("could not remove {}: {error}", path.display())),
            }
        }
        if let Some(parent) = parent {
            sync_directory(&parent)
                .map_err(|error| format!("could not sync {}: {error}", parent.display()))?;
        }
        Ok(removed)
    }
}

fn unavailable_message(key: &SessionKey, reason: &str) -> String {
    format!(
        "persistent {} session {:?} is unavailable until reset or recovery: {reason}",
        key.provider, key.session_name
    )
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let parent = path.parent().expect("session artifact has a parent");
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(".session-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| format!("could not create {}: {error}", temporary.display()))?;
        serde_json::to_writer_pretty(&mut file, value)
            .map_err(|error| format!("could not serialize {}: {error}", path.display()))?;
        file.write_all(b"\n")
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("could not sync {}: {error}", temporary.display()))?;
        drop(file);
        fs::rename(&temporary, path)
            .map_err(|error| format!("could not publish {}: {error}", path.display()))?;
        sync_directory(parent)
            .map_err(|error| format!("could not sync {}: {error}", parent.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn sync_parent(path: &Path) -> Result<(), String> {
    let parent = path.parent().expect("session artifact has a parent");
    sync_directory(parent).map_err(|error| format!("could not sync {}: {error}", parent.display()))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_: &Path) -> std::io::Result<()> {
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct DurableSession {
    key: SessionKey,
    snapshot: SessionSnapshot,
}

#[derive(Serialize, Deserialize)]
struct DurablePending {
    key: SessionKey,
    source_epoch: Option<uuid::Uuid>,
    source_sequence: u64,
    source_ordinal: u16,
}

#[async_trait]
impl AgentBackend for ManagedAgents {
    async fn run_fresh(
        &self,
        context: AgentCallContext,
        provider: &str,
        prompt: Option<String>,
        model: Option<String>,
    ) -> Result<Value, String> {
        let provider = Self::provider(provider)?;
        let model = Self::model(provider, model)?;
        let input = self.input(&context, prompt)?;
        let response = self
            .sessions
            .run_fresh_with_options(provider, input, model, Self::environment(&context))
            .await
            .map_err(|error| error.to_string())?;
        serde_json::to_value(response).map_err(|error| error.to_string())
    }

    async fn send_persistent(
        &self,
        context: AgentCallContext,
        provider: &str,
        session_name: &str,
        prompt: Option<String>,
        model: Option<String>,
    ) -> Result<Value, String> {
        let provider = Self::provider(provider)?;
        let model = Self::model(provider, model)?;
        let key = Self::key(&context, provider, session_name);
        let operation = self.operation_lock(&key).await;
        let _guard = operation.lock().await;
        self.ensure_ready_and_restore(&key).await?;
        let input = self.input(&context, prompt)?;
        let prepare_key = key.clone();
        let commit_key = key.clone();
        let response = self
            .sessions
            .send_persistent_transactional_with_model(
                key.clone(),
                input.clone(),
                model,
                Self::environment(&context),
                || async { self.begin_pending(&prepare_key, &input) },
                |snapshot| async { self.commit_snapshot(&commit_key, snapshot) },
            )
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if self.pending_path(&key).exists() {
                    let reason = self
                        .pending_reason(&key)?
                        .unwrap_or_else(|| error.to_string());
                    self.sessions
                        .mark_session_unavailable(key.clone(), reason)
                        .await
                        .map_err(|mark_error| mark_error.to_string())?;
                }
                return Err(error.to_string());
            }
        };
        serde_json::to_value(response).map_err(|error| error.to_string())
    }

    async fn reset(
        &self,
        context: AgentCallContext,
        provider: &str,
        session_name: &str,
    ) -> Result<Value, String> {
        let key = Self::key(&context, Self::provider(provider)?, session_name);
        let operation = self.operation_lock(&key).await;
        let _guard = operation.lock().await;
        let disk = self.remove_artifacts(&key)?;
        let memory = self.sessions.reset_session(&key).await;
        Ok(json!({"reset": memory || disk}))
    }

    async fn status(
        &self,
        context: AgentCallContext,
        provider: &str,
        session_name: &str,
    ) -> Result<Value, String> {
        let key = Self::key(&context, Self::provider(provider)?, session_name);
        let operation = self.operation_lock(&key).await;
        let _guard = operation.lock().await;
        self.ensure_ready_and_restore(&key).await?;
        serde_json::to_value(
            self.sessions
                .session_status(&key)
                .await
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())
    }
}

fn create_watcher(
    sender: tokio::sync::mpsc::UnboundedSender<notify::Result<notify::Event>>,
) -> notify::Result<RecommendedWatcher> {
    notify::recommended_watcher(move |result| {
        let _ = sender.send(result);
    })
}

fn watch_event_requires_refresh(event: notify::Result<notify::Event>) -> bool {
    match event {
        Ok(event) => !matches!(event.kind, notify::EventKind::Access(_)),
        Err(error) => {
            tracing::error!(%error, "hook watcher reported an error");
            false
        }
    }
}

#[derive(Clone, Default)]
struct SkillRefreshTargets {
    paths: Arc<RwLock<BTreeSet<PathBuf>>>,
}

impl SkillRefreshTargets {
    async fn from_handle(
        handle: &Handle,
        fallback: &Path,
    ) -> Result<Self, codex_control::ThreadListError> {
        let targets = Self::default();
        if let Some(path) = std::env::current_dir()
            .ok()
            .or_else(|| canonical(fallback).ok())
        {
            targets.insert(path).await;
        }
        targets.refresh_from_handle(handle).await?;
        Ok(targets)
    }

    async fn insert(&self, path: PathBuf) -> bool {
        if !path.is_absolute() || path.to_str().is_none() {
            return false;
        }
        self.paths.write().await.insert(path)
    }

    async fn observe_snapshot_threads(&self, threads: &[Value]) -> bool {
        let mut changed = false;
        for thread in threads {
            if let Some(cwd) = thread.get("cwd").and_then(Value::as_str) {
                changed |= self.insert(PathBuf::from(cwd)).await;
            }
        }
        changed
    }

    async fn observe_listed_threads(&self, threads: &[ListedThread]) -> bool {
        let mut changed = false;
        for thread in threads {
            changed |= self.insert(thread.cwd.clone()).await;
        }
        changed
    }

    async fn refresh_from_handle(
        &self,
        handle: &Handle,
    ) -> Result<bool, codex_control::ThreadListError> {
        let threads = handle.list_threads().await?;
        Ok(self.observe_listed_threads(&threads).await)
    }

    async fn observe_source(&self, source: &codex_control::SequencedEvent) -> bool {
        let Some(params) = source.frame.params() else {
            return false;
        };
        let cwd = match source.method() {
            Some("thread/started") => params.pointer("/thread/cwd").or_else(|| params.get("cwd")),
            Some("thread/settings/updated" | "turn/started") => params
                .get("cwd")
                .or_else(|| params.pointer("/settings/cwd"))
                .or_else(|| params.pointer("/turn/cwd")),
            _ => None,
        };
        match cwd.and_then(Value::as_str) {
            Some(cwd) => self.insert(PathBuf::from(cwd)).await,
            None => false,
        }
    }

    async fn all(&self) -> Vec<PathBuf> {
        self.paths.read().await.iter().cloned().collect()
    }
}

async fn refresh_for_new_task_cwd(
    handle: &Handle,
    targets: &SkillRefreshTargets,
    source: &codex_control::SequencedEvent,
) {
    if targets.observe_source(source).await
        && let Err(error) = handle.force_refresh_skills(targets.all().await).await
    {
        tracing::error!(%error, "skill refresh for newly observed task CWD failed");
    }
}

#[derive(Clone, Default)]
struct RuntimeMetrics {
    last_sequence: Arc<AtomicU64>,
    coverage_gap_count: Arc<AtomicUsize>,
}

impl RuntimeMetrics {
    fn record(&self, last_sequence: u64, coverage_gap_count: usize) {
        self.last_sequence.store(last_sequence, Ordering::Release);
        self.coverage_gap_count
            .store(coverage_gap_count, Ordering::Release);
    }

    fn snapshot(&self) -> (u64, usize) {
        (
            self.last_sequence.load(Ordering::Acquire),
            self.coverage_gap_count.load(Ordering::Acquire),
        )
    }
}

fn canonical(path: &Path) -> Result<PathBuf, RuntimeError> {
    fs::canonicalize(path).map_err(|source| RuntimeError::Io {
        path: path.to_owned(),
        source,
    })
}

fn warden_cli_path() -> PathBuf {
    let executable = std::env::current_exe().ok();
    let parent = executable.as_deref().and_then(Path::parent);
    parent
        .into_iter()
        .map(|parent| parent.join("warden"))
        .chain(
            parent
                .filter(|parent| parent.file_name().and_then(|name| name.to_str()) == Some("deps"))
                .and_then(Path::parent)
                .into_iter()
                .map(|parent| parent.join("warden")),
        )
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("warden"))
}

fn log_registry_delta(delta: &crate::registry::RegistryDelta) {
    if !delta.published.is_empty() || !delta.removed.is_empty() {
        tracing::info!(published = ?delta.published, removed = ?delta.removed, "Warden hook registry changed");
    }
    for (hook, error) in &delta.failed {
        tracing::error!(%hook, %error, "hook candidate rejected; last valid revision preserved");
    }
}

async fn update_daemon_health(
    gateway: &ActionGateway,
    registry: &HookRegistry,
    metrics: (u64, usize),
) {
    let (last_sequence, gap_count) = metrics;
    gateway
        .merge_daemon_health(json!({
            "hooks_ready": registry.all().await.len(),
            "hook_failures": registry.failures().await.into_iter().map(|(id, error)| (id.to_string(), error)).collect::<std::collections::HashMap<_, _>>(),
            "last_processed_sequence": last_sequence,
            "coverage_gap_count": gap_count,
        }))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        action::{ActionGrant, InvocationCredential},
        event::{HookEventEnvelope, HookEventKind},
        registry::HookId,
    };
    use async_trait::async_trait;
    use codex_control::{IncomingFrame, SequencedEvent};
    use serde_json::json;
    use std::{
        collections::HashSet,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tempfile::TempDir;
    use tokio::sync::Semaphore;
    use warden_agent::{AgentError, AgentRequest, AgentResponse, Conversation, ResumeMetadata};

    #[test]
    fn warden_startup_does_not_reconcile_historical_tasks() {
        let config = control_config(false);
        assert_eq!(config.ingest.reconcile_candidate_limit, 0);
    }

    struct SessionDriver {
        calls: AtomicUsize,
        started: Option<Arc<Semaphore>>,
        release: Option<Arc<Semaphore>>,
    }

    impl SessionDriver {
        fn immediate() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                started: None,
                release: None,
            })
        }

        fn blocked(started: Arc<Semaphore>, release: Arc<Semaphore>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                started: Some(started),
                release: Some(release),
            })
        }
    }

    #[async_trait]
    impl ProviderDriver for SessionDriver {
        fn provider(&self) -> ProviderKind {
            ProviderKind::Claude
        }

        async fn invoke(&self, request: AgentRequest) -> Result<AgentResponse, AgentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let (Some(started), Some(release)) = (&self.started, &self.release) {
                started.add_permits(1);
                release
                    .acquire()
                    .await
                    .expect("release semaphore open")
                    .forget();
            }
            let resume = match request.conversation {
                Conversation::Fresh => None,
                Conversation::Persistent { resume } => Some(resume.unwrap_or_else(|| {
                    ResumeMetadata::new(ProviderKind::Claude, "durable-session")
                })),
            };
            Ok(AgentResponse {
                provider: ProviderKind::Claude,
                invocation_id: request.invocation_id,
                source_sequence: request.input.source_sequence,
                text: Some("ok".into()),
                structured_output: None,
                events: vec![json!({"type":"result"})],
                usage: None,
                resume,
            })
        }

        async fn interrupt(&self, _: uuid::Uuid) -> Result<bool, AgentError> {
            Ok(false)
        }

        async fn shutdown(&self) -> Result<(), AgentError> {
            Ok(())
        }
    }

    fn managed_with_driver(root: &Path, driver: Arc<SessionDriver>) -> Arc<ManagedAgents> {
        let registered: Arc<dyn ProviderDriver> = driver;
        Arc::new(ManagedAgents::new(
            Arc::new(AgentSessions::new([registered]).expect("unique provider")),
            root.to_owned(),
        ))
    }

    fn agent_context(sequence: u64) -> AgentCallContext {
        AgentCallContext {
            hook_id: HookId::parse("durability-test").unwrap(),
            source_thread_id: "thread".into(),
            event: HookEventEnvelope {
                sequence,
                origin: crate::event::HookEventOriginKind::Observed,
                source_sequence: Some(sequence),
                receipt_ordinal: sequence,
                native_event_name: None,
                kind: HookEventKind::AgentMessageCompleted,
                thread_id: Some("thread".into()),
                turn_id: Some("turn".into()),
                item_id: Some(format!("item-{sequence}")),
                unix_receipt_ms: sequence,
                emitted_at_ms: None,
                reconstructed: false,
                payload: json!({"sequence":sequence}),
                raw_method: Some("item/completed".into()),
                raw_payload: json!({}),
            },
            credential: InvocationCredential {
                invocation_id: uuid::Uuid::new_v4(),
                token: "credential".into(),
                socket: PathBuf::from("/tmp/warden.sock"),
            },
            grant: ActionGrant::default(),
        }
    }

    #[test]
    fn provider_catalog_contains_both_local_clis() {
        let providers = agent_drivers(&Config::default())
            .into_iter()
            .map(|driver| driver.provider())
            .collect::<HashSet<_>>();
        assert_eq!(
            providers,
            HashSet::from([ProviderKind::Claude, ProviderKind::Codex])
        );
    }

    #[test]
    fn agent_action_credential_uses_a_codex_shell_safe_name() {
        let context = AgentCallContext {
            hook_id: HookId::parse("credential-test").unwrap(),
            source_thread_id: "thread".into(),
            event: HookEventEnvelope {
                sequence: 1,
                origin: crate::event::HookEventOriginKind::Observed,
                source_sequence: Some(1),
                receipt_ordinal: 1,
                native_event_name: None,
                kind: HookEventKind::TurnStarted,
                thread_id: Some("thread".into()),
                turn_id: Some("turn".into()),
                item_id: None,
                unix_receipt_ms: 1,
                emitted_at_ms: None,
                reconstructed: false,
                payload: json!({}),
                raw_method: Some("turn/started".into()),
                raw_payload: json!({}),
            },
            credential: InvocationCredential {
                invocation_id: uuid::Uuid::new_v4(),
                token: "secret-value".into(),
                socket: PathBuf::from("/tmp/warden.sock"),
            },
            grant: ActionGrant::default(),
        };
        let environment = ManagedAgents::environment(&context);
        assert_eq!(
            environment.get("WARDEN_INVOCATION_AUTH"),
            Some(std::ffi::OsStr::new("secret-value"))
        );
        assert!(environment.get("WARDEN_INVOCATION_TOKEN").is_none());
        assert!(!format!("{environment:?}").contains("secret-value"));
    }

    #[tokio::test]
    async fn failed_session_publication_is_quarantined_across_restart_until_reset() {
        let temp = TempDir::new().unwrap();
        let started = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let first_driver = SessionDriver::blocked(started.clone(), release.clone());
        let first = managed_with_driver(temp.path(), first_driver.clone());
        let first_context = agent_context(10);
        let key = ManagedAgents::key(&first_context, ProviderKind::Claude, "monitor");

        let sending = {
            let first = first.clone();
            tokio::spawn(async move {
                first
                    .send_persistent(first_context, "claude", "monitor", None, None)
                    .await
            })
        };
        started.acquire().await.unwrap().forget();
        assert!(first.pending_path(&key).is_file());

        // A directory at the final record path deterministically makes atomic rename fail after
        // the provider has returned, without preventing the pending marker from being durable.
        let record_path = first.record_path(&key);
        fs::create_dir(&record_path).unwrap();
        release.add_permits(1);
        let error = sending
            .await
            .unwrap()
            .expect_err("publication failure must fail the send");
        assert!(error.contains("durable commit failed"), "{error}");
        assert!(first.pending_path(&key).is_file());
        fs::remove_dir(&record_path).unwrap();

        let rejected = first
            .send_persistent(agent_context(11), "claude", "monitor", None, None)
            .await
            .expect_err("same-daemon continuation must be rejected");
        assert!(rejected.contains("unavailable until reset or recovery"));
        assert_eq!(first_driver.calls.load(Ordering::SeqCst), 1);
        assert!(
            first
                .status(agent_context(11), "claude", "monitor")
                .await
                .expect_err("status must expose quarantine")
                .contains("unavailable until reset or recovery")
        );

        let restarted_driver = SessionDriver::immediate();
        let restarted = managed_with_driver(temp.path(), restarted_driver.clone());
        let restart_error = restarted
            .status(agent_context(12), "claude", "monitor")
            .await
            .expect_err("restart must detect the pending marker");
        assert!(restart_error.contains("did not commit"), "{restart_error}");
        assert!(
            restarted
                .send_persistent(agent_context(12), "claude", "monitor", None, None)
                .await
                .expect_err("restart must not resume stale durable state")
                .contains("unavailable until reset or recovery")
        );
        assert_eq!(restarted_driver.calls.load(Ordering::SeqCst), 0);

        let reset = restarted
            .reset(agent_context(12), "claude", "monitor")
            .await
            .expect("explicit reset clears quarantine");
        assert_eq!(reset, json!({"reset":true}));
        assert!(!restarted.pending_path(&key).exists());

        restarted
            .send_persistent(agent_context(13), "claude", "monitor", None, None)
            .await
            .expect("send succeeds after reset");
        assert_eq!(restarted_driver.calls.load(Ordering::SeqCst), 1);
        assert!(restarted.record_path(&key).is_file());
        assert!(!restarted.pending_path(&key).exists());
        assert!(
            restarted
                .status(agent_context(13), "claude", "monitor")
                .await
                .expect("committed session status")
                .is_object()
        );

        let verified_driver = SessionDriver::immediate();
        let verified = managed_with_driver(temp.path(), verified_driver.clone());
        assert!(
            verified
                .status(agent_context(14), "claude", "monitor")
                .await
                .expect("a clean restart restores the committed snapshot")
                .is_object()
        );
        assert_eq!(verified_driver.calls.load(Ordering::SeqCst), 0);
        assert!(
            fs::read_dir(verified.record_path(&key).parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".session-"))
        );
    }

    #[tokio::test]
    async fn persistent_agent_cursor_orders_mixed_native_and_observed_event_spaces() {
        let temp = TempDir::new().unwrap();
        let driver = SessionDriver::immediate();
        let agents = managed_with_driver(temp.path(), driver.clone());

        agents
            .send_persistent(
                agent_context(447),
                "claude",
                "mixed-events",
                None,
                Some("sonnet".into()),
            )
            .await
            .expect("high observed sequence starts the conversation");
        agents
            .send_persistent(
                agent_context(1),
                "claude",
                "mixed-events",
                None,
                Some("sonnet".into()),
            )
            .await
            .expect("low native receipt sequence remains a later agent invocation");

        assert_eq!(driver.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn skill_refresh_targets_follow_started_and_settings_cwds() {
        let temp = TempDir::new().unwrap();
        let first = temp.path().join("first-task");
        let second = temp.path().join("second-task");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let targets = SkillRefreshTargets::default();

        let started = SequencedEvent::from_frame(
            1,
            Duration::from_millis(1),
            IncomingFrame::parse(json!({
                "jsonrpc":"2.0",
                "method":"thread/started",
                "params":{"thread":{"id":"thread","cwd":first}}
            }))
            .unwrap(),
        );
        assert!(targets.observe_source(&started).await);
        assert!(!targets.observe_source(&started).await);

        let settings = SequencedEvent::from_frame(
            2,
            Duration::from_millis(2),
            IncomingFrame::parse(json!({
                "jsonrpc":"2.0",
                "method":"thread/settings/updated",
                "params":{"threadId":"thread","cwd":second}
            }))
            .unwrap(),
        );
        assert!(targets.observe_source(&settings).await);
        assert_eq!(targets.all().await, vec![first, second]);
    }
}
