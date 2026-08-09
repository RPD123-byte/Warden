use codex_control::{CodexControl, Config as ControlConfig};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    future::pending,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Semaphore, watch},
};
use transport::mock::{Fault, MockAppServer, MockThread};
use warden_daemon::{
    HookId,
    action::{
        ACTION_PROTOCOL_VERSION, ActionGateway, ActionGrant, ActionKind, AgentBackend,
        AgentCallContext, GatewayRequest, NoAgentBackend, RequestCredential,
    },
    event::{HookEventEnvelope, HookEventKind},
};

fn source_event() -> HookEventEnvelope {
    HookEventEnvelope {
        sequence: 7,
        origin: warden_daemon::event::HookEventOriginKind::Observed,
        source_sequence: Some(7),
        receipt_ordinal: 7,
        native_event_name: None,
        kind: HookEventKind::PostToolUse,
        thread_id: Some("current".into()),
        turn_id: Some("turn".into()),
        item_id: Some("tool".into()),
        unix_receipt_ms: 1,
        emitted_at_ms: None,
        reconstructed: false,
        payload: json!({"result":"ok"}),
        raw_method: Some("item/completed".into()),
        raw_payload: json!({"method":"item/completed"}),
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct BlockingAgentBackend {
    started: Arc<Semaphore>,
    cancelled: Arc<AtomicBool>,
}

struct DelayedAgentBackend;

#[async_trait::async_trait]
impl AgentBackend for DelayedAgentBackend {
    async fn run_fresh(
        &self,
        _: AgentCallContext,
        _: &str,
        _: Option<String>,
    ) -> Result<Value, String> {
        tokio::time::sleep(Duration::from_millis(75)).await;
        Ok(json!({"finished": true}))
    }

    async fn send_persistent(
        &self,
        _: AgentCallContext,
        _: &str,
        _: &str,
        _: Option<String>,
    ) -> Result<Value, String> {
        Err("not used".into())
    }

    async fn reset(&self, _: AgentCallContext, _: &str, _: &str) -> Result<Value, String> {
        Err("not used".into())
    }

    async fn status(&self, _: AgentCallContext, _: &str, _: &str) -> Result<Value, String> {
        Err("not used".into())
    }
}

#[async_trait::async_trait]
impl AgentBackend for BlockingAgentBackend {
    async fn run_fresh(
        &self,
        _: AgentCallContext,
        _: &str,
        _: Option<String>,
    ) -> Result<Value, String> {
        let _cancel = CancelOnDrop(self.cancelled.clone());
        self.started.add_permits(1);
        pending::<()>().await;
        unreachable!()
    }

    async fn send_persistent(
        &self,
        _: AgentCallContext,
        _: &str,
        _: &str,
        _: Option<String>,
    ) -> Result<Value, String> {
        Err("not used".into())
    }

    async fn reset(&self, _: AgentCallContext, _: &str, _: &str) -> Result<Value, String> {
        Err("not used".into())
    }

    async fn status(&self, _: AgentCallContext, _: &str, _: &str) -> Result<Value, String> {
        Err("not used".into())
    }
}

fn request(
    credential: &warden_daemon::action::InvocationCredential,
    name: &str,
    arguments: Value,
) -> GatewayRequest {
    GatewayRequest {
        message_type: "request".into(),
        protocol_version: ACTION_PROTOCOL_VERSION,
        id: format!("request-{name}"),
        method: "warden.action".into(),
        params: json!({"name":name,"arguments":arguments}),
        context: Some(RequestCredential {
            invocation_id: credential.invocation_id,
            token: credential.token.clone(),
        }),
        bridge_auth: None,
    }
}

fn control_config(socket: PathBuf) -> ControlConfig {
    let mut config = ControlConfig {
        manage_gui: false,
        ..ControlConfig::default()
    };
    config.transport.socket_path = socket;
    config.transport.connect_timeout = Duration::from_millis(300);
    config.transport.request_timeout = Duration::from_secs(1);
    config.transport.retry_initial = Duration::from_millis(20);
    config.transport.retry_max = Duration::from_millis(50);
    config.control.correlation_window = Duration::from_millis(100);
    config.control.not_written_retries = 0;
    config
}

#[tokio::test]
async fn grants_scope_and_underlying_action_outcomes_are_preserved() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let action_socket = temp.path().join("warden.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    server
        .add_thread(MockThread {
            id: "current".into(),
            cwd: PathBuf::from("/mock/current"),
            status: "active".into(),
            turn_id: Some("turn".into()),
            ephemeral: false,
            updated_at: 2,
        })
        .await;
    server
        .add_thread(MockThread {
            id: "other".into(),
            cwd: PathBuf::from("/mock/other"),
            status: "active".into(),
            turn_id: Some("other-turn".into()),
            ephemeral: false,
            updated_at: 1,
        })
        .await;
    server
        .add_thread(MockThread {
            id: "idle".into(),
            cwd: PathBuf::from("/mock/idle"),
            status: "idle".into(),
            turn_id: None,
            ephemeral: false,
            updated_at: 0,
        })
        .await;

    let server_for_run = server.clone();
    CodexControl::run(control_config(rpc_socket), move |handle| async move {
        let gateway = ActionGateway::new(handle, action_socket, Arc::new(NoAgentBackend));
        let no_actions = gateway
            .register_invocation(
                HookId::parse("none").unwrap(),
                source_event(),
                ActionGrant::default(),
            )
            .await
            .unwrap();
        let denied = gateway
            .dispatch(request(&no_actions, "current_event", json!({})))
            .await;
        assert!(!denied.ok);
        assert_eq!(denied.error.unwrap().code, "access_denied");

        let current_grant = ActionGrant {
            actions: HashSet::from([
                ActionKind::CurrentEvent,
                ActionKind::CurrentThreadSnapshot,
                ActionKind::TurnInterrupt,
                ActionKind::TurnSteer,
            ]),
        };
        let current = gateway
            .register_invocation(
                HookId::parse("current").unwrap(),
                source_event(),
                current_grant,
            )
            .await
            .unwrap();
        let event = gateway
            .dispatch(request(&current, "current_event", json!({})))
            .await;
        assert_eq!(event.result.unwrap()["sequence"], 7);
        let target_escape = gateway
            .dispatch(request(
                &current,
                "current_thread_snapshot",
                json!({"thread_id":"other"}),
            ))
            .await;
        assert_eq!(target_escape.error.unwrap().code, "access_denied");
        let not_granted_list = gateway
            .dispatch(request(&current, "thread_list", json!({})))
            .await;
        assert_eq!(not_granted_list.error.unwrap().code, "access_denied");

        let cross = gateway
            .register_invocation(
                HookId::parse("cross").unwrap(),
                source_event(),
                ActionGrant {
                    actions: HashSet::from([
                        ActionKind::ThreadList,
                        ActionKind::ArbitraryThreadSnapshot,
                    ]),
                },
            )
            .await
            .unwrap();
        let listed = gateway
            .dispatch(request(&cross, "thread_list", json!({})))
            .await;
        let listed = listed.result.unwrap();
        assert!(listed.get("current").is_some());
        assert_eq!(listed["idle"]["cwd"], "/mock/idle");
        assert_eq!(listed["idle"]["status"]["type"], "idle");
        let other = gateway
            .dispatch(request(
                &cross,
                "arbitrary_thread_snapshot",
                json!({"thread_id":"other"}),
            ))
            .await;
        assert_eq!(other.result.unwrap()["thread"]["thread_id"], "other");

        server_for_run
            .set_fault(Fault::DisconnectOnMethod("turn/steer".into()))
            .await;
        let ambiguous = gateway
            .dispatch(request(
                &current,
                "turn_steer",
                json!({"input":[{"type":"text","text":"continue"}]}),
            ))
            .await;
        assert!(ambiguous.ok);
        assert!(ambiguous.result.unwrap().get("OutcomeUnknown").is_some());
        server_for_run.set_fault(Fault::None).await;

        let unknown = gateway
            .dispatch(request(&current, "run_shell", json!({})))
            .await;
        assert_eq!(unknown.error.unwrap().code, "unknown_action");

        let mut bad_request = request(&current, "current_event", json!({}));
        bad_request
            .context
            .as_mut()
            .unwrap()
            .token
            .push_str("wrong");
        let unauthorized = gateway.dispatch(bad_request).await;
        assert_eq!(unauthorized.error.unwrap().code, "unauthorized");
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn private_socket_and_cli_are_thin_clients_of_the_gateway() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let action_socket = temp.path().join("warden.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    server
        .add_thread(MockThread {
            id: "current".into(),
            cwd: PathBuf::from("/mock/current"),
            status: "active".into(),
            turn_id: Some("turn".into()),
            ephemeral: false,
            updated_at: 1,
        })
        .await;

    CodexControl::run(control_config(rpc_socket), move |handle| async move {
        let gateway = ActionGateway::new(handle, action_socket.clone(), Arc::new(NoAgentBackend));
        let credential = gateway
            .register_invocation(
                HookId::parse("socket").unwrap(),
                source_event(),
                ActionGrant {
                    actions: HashSet::from([ActionKind::CurrentEvent]),
                },
            )
            .await
            .unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let serving = {
            let gateway = gateway.clone();
            tokio::spawn(async move { gateway.serve(shutdown_rx, 64 * 1024).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !action_socket.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&action_socket)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let wire_request = request(&credential, "current_event", json!({}));
        let mut encoded = serde_json::to_vec(&wire_request).unwrap();
        encoded.push(b'\n');
        let stream = tokio::net::UnixStream::connect(&action_socket)
            .await
            .unwrap();
        let (reader, mut writer) = stream.into_split();
        writer.write_all(&encoded).await.unwrap();
        let mut response = String::new();
        BufReader::new(reader)
            .read_line(&mut response)
            .await
            .unwrap();
        let response: warden_daemon::action::GatewayResponse =
            serde_json::from_str(&response).unwrap();
        assert!(response.ok);
        assert_eq!(response.result.unwrap()["sequence"], 7);

        let cli = env!("CARGO_BIN_EXE_warden");
        let health = tokio::process::Command::new(cli)
            .args(["--socket", action_socket.to_str().unwrap(), "health"])
            .output()
            .await
            .unwrap();
        assert!(
            health.status.success(),
            "{}",
            String::from_utf8_lossy(&health.stderr)
        );
        let health_json: Value = serde_json::from_slice(&health.stdout).unwrap();
        assert_eq!(health_json["phase"], "connected");

        let action = tokio::process::Command::new(cli)
            .args([
                "--socket",
                action_socket.to_str().unwrap(),
                "--invocation-id",
                &credential.invocation_id.to_string(),
                "--token",
                &credential.token,
                "action",
                "current_event",
            ])
            .output()
            .await
            .unwrap();
        assert!(
            action.status.success(),
            "{}",
            String::from_utf8_lossy(&action.stderr)
        );
        let event_json: Value = serde_json::from_slice(&action.stdout).unwrap();
        assert_eq!(event_json["sequence"], 7);

        let inherited_action = tokio::process::Command::new(cli)
            .args([
                "--socket",
                action_socket.to_str().unwrap(),
                "--invocation-id",
                &credential.invocation_id.to_string(),
                "action",
                "current_event",
            ])
            .env("WARDEN_INVOCATION_AUTH", &credential.token)
            .env_remove("WARDEN_INVOCATION_TOKEN")
            .output()
            .await
            .unwrap();
        assert!(
            inherited_action.status.success(),
            "{}",
            String::from_utf8_lossy(&inherited_action.stderr)
        );
        let inherited_event: Value = serde_json::from_slice(&inherited_action.stdout).unwrap();
        assert_eq!(inherited_event["sequence"], 7);

        let _ = shutdown_tx.send(true);
        serving.await.unwrap().unwrap();
        assert!(!action_socket.exists());
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn closing_a_hook_client_cancels_its_nested_agent_call() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let action_socket = temp.path().join("warden.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    server
        .add_thread(MockThread {
            id: "current".into(),
            cwd: PathBuf::from("/mock/current"),
            status: "active".into(),
            turn_id: Some("turn".into()),
            ephemeral: false,
            updated_at: 1,
        })
        .await;

    CodexControl::run(control_config(rpc_socket), move |handle| async move {
        let started = Arc::new(Semaphore::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let backend = Arc::new(BlockingAgentBackend {
            started: started.clone(),
            cancelled: cancelled.clone(),
        });
        let gateway = ActionGateway::new(handle, action_socket.clone(), backend);
        let credential = gateway
            .register_invocation(
                HookId::parse("nested-agent").unwrap(),
                source_event(),
                ActionGrant::default(),
            )
            .await
            .unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let serving = {
            let gateway = gateway.clone();
            tokio::spawn(async move { gateway.serve(shutdown_rx, 64 * 1024).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !action_socket.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        let request = GatewayRequest {
            message_type: "request".into(),
            protocol_version: ACTION_PROTOCOL_VERSION,
            id: "agent-request".into(),
            method: "agent.run".into(),
            params: json!({
                "provider":"claude",
                "prompt":"wait",
                "event": source_event(),
            }),
            context: Some(RequestCredential {
                invocation_id: credential.invocation_id,
                token: credential.token,
            }),
            bridge_auth: None,
        };
        let mut encoded = serde_json::to_vec(&request).unwrap();
        encoded.push(b'\n');
        let mut stream = tokio::net::UnixStream::connect(&action_socket)
            .await
            .unwrap();
        stream.write_all(&encoded).await.unwrap();
        started.acquire().await.unwrap().forget();
        drop(stream);

        tokio::time::timeout(Duration::from_secs(1), async {
            while !cancelled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect should drop the nested agent future");

        cancelled.store(false, Ordering::Release);
        let revoked = gateway
            .register_invocation(
                HookId::parse("revoked-agent").unwrap(),
                source_event(),
                ActionGrant::default(),
            )
            .await
            .unwrap();
        let revoked_id = revoked.invocation_id;
        let revoked_request = GatewayRequest {
            message_type: "request".into(),
            protocol_version: ACTION_PROTOCOL_VERSION,
            id: "revoked-request".into(),
            method: "agent.run".into(),
            params: json!({
                "provider":"claude",
                "prompt":"wait",
                "event": source_event(),
            }),
            context: Some(RequestCredential {
                invocation_id: revoked.invocation_id,
                token: revoked.token,
            }),
            bridge_auth: None,
        };
        let dispatch_gateway = gateway.clone();
        let dispatched =
            tokio::spawn(async move { dispatch_gateway.dispatch(revoked_request).await });
        started.acquire().await.unwrap().forget();
        gateway.revoke_invocation(revoked_id).await;
        let response = dispatched.await.unwrap();
        assert_eq!(response.error.unwrap().code, "invocation_cancelled");
        assert!(cancelled.load(Ordering::Acquire));

        let _ = shutdown_tx.send(true);
        serving.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn write_half_close_does_not_cancel_an_owned_nested_agent_call() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let action_socket = temp.path().join("warden.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();

    CodexControl::run(control_config(rpc_socket), move |handle| async move {
        let gateway =
            ActionGateway::new(handle, action_socket.clone(), Arc::new(DelayedAgentBackend));
        let credential = gateway
            .register_invocation(
                HookId::parse("half-close").unwrap(),
                source_event(),
                ActionGrant::default(),
            )
            .await
            .unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let serving = {
            let gateway = gateway.clone();
            tokio::spawn(async move { gateway.serve(shutdown_rx, 64 * 1024).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !action_socket.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        let request = GatewayRequest {
            message_type: "request".into(),
            protocol_version: ACTION_PROTOCOL_VERSION,
            id: "half-close-request".into(),
            method: "agent.run".into(),
            params: json!({
                "provider":"claude",
                "prompt":"finish",
                "event": source_event(),
            }),
            context: Some(RequestCredential {
                invocation_id: credential.invocation_id,
                token: credential.token,
            }),
            bridge_auth: None,
        };
        let mut encoded = serde_json::to_vec(&request).unwrap();
        encoded.push(b'\n');
        let mut stream = tokio::net::UnixStream::connect(&action_socket)
            .await
            .unwrap();
        stream.write_all(&encoded).await.unwrap();
        stream.shutdown().await.unwrap();
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .await
            .unwrap();
        let response: warden_daemon::action::GatewayResponse =
            serde_json::from_str(&response).unwrap();
        assert!(response.ok);
        assert_eq!(response.result.unwrap(), json!({"finished": true}));

        let _ = shutdown_tx.send(true);
        serving.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn oversized_gateway_results_return_a_bounded_structured_error() {
    const MAX_BYTES: usize = 512;
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let action_socket = temp.path().join("warden.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();

    CodexControl::run(control_config(rpc_socket), move |handle| async move {
        let gateway = ActionGateway::new(handle, action_socket.clone(), Arc::new(NoAgentBackend));
        let mut event = source_event();
        event.payload = json!({"large": "x".repeat(16 * 1024)});
        let credential = gateway
            .register_invocation(
                HookId::parse("oversized").unwrap(),
                event,
                ActionGrant {
                    actions: HashSet::from([ActionKind::CurrentEvent]),
                },
            )
            .await
            .unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let serving = {
            let gateway = gateway.clone();
            tokio::spawn(async move { gateway.serve(shutdown_rx, MAX_BYTES).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !action_socket.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        let mut encoded =
            serde_json::to_vec(&request(&credential, "current_event", json!({}))).unwrap();
        encoded.push(b'\n');
        assert!(encoded.len() <= MAX_BYTES);
        let mut stream = tokio::net::UnixStream::connect(&action_socket)
            .await
            .unwrap();
        stream.write_all(&encoded).await.unwrap();
        let mut response = Vec::new();
        BufReader::new(stream)
            .read_until(b'\n', &mut response)
            .await
            .unwrap();
        assert!(response.len() <= MAX_BYTES);
        assert!(response.ends_with(b"\n"));
        let response: warden_daemon::action::GatewayResponse =
            serde_json::from_slice(&response).unwrap();
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "response_too_large");

        let _ = shutdown_tx.send(true);
        serving.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    server.shutdown().await;
}
