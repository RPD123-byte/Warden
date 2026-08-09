#![cfg(unix)]

use async_trait::async_trait;
use codex_control::{CodexControl, Config as ControlConfig};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::sync::watch;
use transport::mock::{MockAppServer, MockThread};
use warden_agent::{
    AgentInput, AgentSessions, ClaudeCliDriver, CliConfig, InvocationEnvironment, ProviderDriver,
    ProviderKind, SessionKey,
};
use warden_daemon::{
    Config, DataPaths, HookRegistry,
    action::{
        ACTION_PROTOCOL_VERSION, ActionGateway, AgentBackend, AgentCallContext, GatewayRequest,
    },
    activation::ActivationRouter,
    native_hook::BRIDGE_EVENTS,
    python::PythonRuntime,
    reconcile_codex,
};

const FAKE_CLAUDE: &str = r#"#!/bin/sh
set -eu
input=$(cat)
printf '%s\n' "$*" >> "$ARGS_LOG"
printf '%s\n' "$input" >> "$INPUT_LOG"

session_id=""
previous=""
for argument in "$@"; do
  if [ "$previous" = "--session-id" ] || [ "$previous" = "--resume" ]; then
    session_id="$argument"
  fi
  previous="$argument"
done

case " $* " in
  *" --session-id "*)
    printf 'history\n' >> "$ACTION_LOG"
    warden action current_thread_history --arguments '{}' >> "$HISTORY_LOG"
    ;;
esac

case "$input" in
  *requires_decision*)
    printf 'steer\n' >> "$ACTION_LOG"
    warden action turn_steer --arguments '{"input":[{"type":"text","text":"Stopped: unspecified architecture choice. Question: Which architecture should Codex use?"}]}' >> "$ACTION_RESULT_LOG"
    printf 'interrupt\n' >> "$ACTION_LOG"
    warden action turn_interrupt --arguments '{}' >> "$ACTION_RESULT_LOG"
    ;;
  *missing_baseline*)
    printf 'steer\n' >> "$ACTION_LOG"
    warden action turn_steer --arguments '{"input":[{"type":"text","text":"Stopped: the initial review baseline is unavailable. Question: Which request or specification governs this implementation?"}]}' >> "$ACTION_RESULT_LOG"
    printf 'interrupt\n' >> "$ACTION_LOG"
    warden action turn_interrupt --arguments '{}' >> "$ACTION_RESULT_LOG"
    ;;
  *)
    printf 'no_action\n' >> "$ACTION_LOG"
    ;;
esac

sleep 0.15
printf '%s\n' "{\"type\":\"system\",\"session_id\":\"$session_id\"}"
printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"review complete\",\"session_id\":\"$session_id\"}"
"#;

#[derive(Clone)]
struct TestAgentBackend {
    sessions: Arc<AgentSessions>,
    cli_parent: PathBuf,
}

impl TestAgentBackend {
    fn input(context: &AgentCallContext, prompt: Option<String>) -> Result<AgentInput, String> {
        let event = serde_json::to_value(&context.event).map_err(|error| error.to_string())?;
        Ok(AgentInput::new(context.event.sequence, event).with_prompt(prompt.unwrap_or_default()))
    }

    fn environment(&self, context: &AgentCallContext) -> InvocationEnvironment {
        let mut paths = vec![self.cli_parent.clone()];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        let path = std::env::join_paths(paths).unwrap();
        InvocationEnvironment::new()
            .with_var("WARDEN_SOCKET", context.credential.socket.as_os_str())
            .with_var(
                "WARDEN_INVOCATION_ID",
                context.credential.invocation_id.to_string(),
            )
            .with_var("WARDEN_INVOCATION_AUTH", &context.credential.token)
            .with_var("PATH", path)
    }

    fn key(context: &AgentCallContext, provider: ProviderKind, name: &str) -> SessionKey {
        SessionKey::new(
            provider,
            context.hook_id.as_str(),
            name,
            &context.source_thread_id,
        )
    }
}

#[async_trait]
impl AgentBackend for TestAgentBackend {
    async fn run_fresh(
        &self,
        context: AgentCallContext,
        provider: &str,
        prompt: Option<String>,
        model: Option<String>,
    ) -> Result<Value, String> {
        if provider != "claude" {
            return Err("only the fake Claude provider is configured".into());
        }
        let response = self
            .sessions
            .run_fresh_with_options(
                ProviderKind::Claude,
                Self::input(&context, prompt)?,
                model,
                self.environment(&context),
            )
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
        if provider != "claude" {
            return Err("only the fake Claude provider is configured".into());
        }
        let response = self
            .sessions
            .send_persistent_with_options(
                Self::key(&context, ProviderKind::Claude, session_name),
                Self::input(&context, prompt)?,
                model,
                self.environment(&context),
            )
            .await
            .map_err(|error| error.to_string())?;
        serde_json::to_value(response).map_err(|error| error.to_string())
    }

    async fn reset(
        &self,
        context: AgentCallContext,
        provider: &str,
        session_name: &str,
    ) -> Result<Value, String> {
        let provider = match provider {
            "claude" => ProviderKind::Claude,
            _ => return Err("only the fake Claude provider is configured".into()),
        };
        Ok(json!({
            "reset": self.sessions.reset_session(&Self::key(&context, provider, session_name)).await
        }))
    }

    async fn status(
        &self,
        context: AgentCallContext,
        provider: &str,
        session_name: &str,
    ) -> Result<Value, String> {
        let provider = match provider {
            "claude" => ProviderKind::Claude,
            _ => return Err("only the fake Claude provider is configured".into()),
        };
        serde_json::to_value(
            self.sessions
                .session_status(&Self::key(&context, provider, session_name))
                .await
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())
    }
}

fn control_config(socket: &Path) -> ControlConfig {
    let mut config = ControlConfig {
        manage_gui: false,
        ..ControlConfig::default()
    };
    config.transport.socket_path = socket.to_owned();
    config.transport.connect_timeout = Duration::from_millis(300);
    config.transport.request_timeout = Duration::from_secs(2);
    config
}

fn native_request(id: &str, params: Value) -> GatewayRequest {
    GatewayRequest {
        message_type: "request".into(),
        protocol_version: ACTION_PROTOCOL_VERSION,
        id: id.into(),
        method: "warden.native_hook.event".into(),
        params,
        context: None,
        bridge_auth: Some("bridge-secret".into()),
    }
}

fn install_fake_claude(temp: &TempDir) -> PathBuf {
    let path = temp.path().join("claude");
    fs::write(&path, FAKE_CLAUDE).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn wait_for(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn template_blocks_native_events_reuses_context_and_orders_stop_actions() {
    let temp = TempDir::new().unwrap();
    let fake_claude = install_fake_claude(&temp);
    let args_log = temp.path().join("args.log");
    let input_log = temp.path().join("input.log");
    let history_log = temp.path().join("history.log");
    let action_log = temp.path().join("actions.log");
    let action_result_log = temp.path().join("action-results.log");
    let rpc_socket = temp.path().join("rpc.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    for id in ["thread-a", "thread-b"] {
        server
            .add_thread(MockThread {
                id: id.into(),
                cwd: temp.path().join(id),
                status: "active".into(),
                turn_id: Some(format!("turn-{id}")),
                ephemeral: false,
                updated_at: 1,
            })
            .await;
    }

    let paths = DataPaths::under(temp.path().join("warden-home"));
    let mut config = Config {
        paths: paths.clone(),
        codex_home: temp.path().join("codex"),
        hook_timeout: Duration::from_secs(8),
        agent_timeout: Duration::from_secs(5),
        ..Config::default()
    };
    config.python_sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
    reconcile_codex(&config).unwrap();

    let cli = CliConfig::new(fake_claude)
        .with_timeout(Duration::from_secs(5))
        .with_env("ARGS_LOG", &args_log)
        .with_env("INPUT_LOG", &input_log)
        .with_env("HISTORY_LOG", &history_log)
        .with_env("ACTION_LOG", &action_log)
        .with_env("ACTION_RESULT_LOG", &action_result_log);
    let driver: Arc<dyn ProviderDriver> = Arc::new(ClaudeCliDriver::new(cli));
    let sessions = Arc::new(AgentSessions::new([driver]).unwrap());
    let backend = Arc::new(TestAgentBackend {
        sessions: sessions.clone(),
        cli_parent: PathBuf::from(env!("CARGO_BIN_EXE_warden"))
            .parent()
            .unwrap()
            .to_owned(),
    });

    let server_for_run = server.clone();
    CodexControl::run(control_config(&rpc_socket), move |handle| async move {
        let python = Arc::new(PythonRuntime::new(&config));
        let registry = HookRegistry::new(
            paths.hooks.clone(),
            paths.modules.clone(),
            paths.generated_skills.clone(),
            paths.runtimes.clone(),
            python.clone(),
        );
        registry.refresh().await.unwrap();
        let marker = paths
            .generated_skills
            .join("unspecified-decisions/SKILL.md");
        let gateway = ActionGateway::new(handle.clone(), paths.action_socket.clone(), backend)
            .with_native_hook_runtime(
                ActivationRouter::new(paths.generated_skills.clone(), registry),
                python,
                "bridge-secret".into(),
                4,
            );
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let serving = {
            let gateway = gateway.clone();
            tokio::spawn(async move { gateway.serve(shutdown_rx, 1024 * 1024).await })
        };
        wait_for(&paths.action_socket).await;

        server_for_run
            .emit_notification(
                "turn/started",
                json!({"threadId":"thread-a","turn":{"id":"turn-thread-a","status":"inProgress","items":[{
                    "id":"initial-a","type":"userMessage","content":[{"type":"text","text":"Initial specification: use the selected architecture only."}]
                }]}}),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let history = handle.query_sequence(Some("thread-a"), 0, None).await;
                if history.events.iter().any(|event| {
                    event
                        .frame
                        .raw()
                        .to_string()
                        .contains("Initial specification")
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let activation = gateway
            .dispatch(native_request(
                "activate-a",
                json!({
                    "hook_event_name":"UserPromptSubmit", "session_id":"thread-a",
                    "turn_id":"turn-thread-a", "cwd":temp.path(),
                    "prompt":format!("[$unspecified-decisions]({}) review", marker.display())
                }),
            ))
            .await;
        assert!(activation.ok, "{activation:?}");

        let started = Instant::now();
        let ordinary = gateway
            .dispatch(native_request(
                "ordinary-post-tool",
                json!({
                    "hook_event_name":"PostToolUse", "session_id":"thread-a",
                    "turn_id":"turn-thread-a", "cwd":temp.path(), "tool_use_id":"tool-1",
                    "tool_name":"shell", "tool_response":{"ok":true}
                }),
            ))
            .await;
        assert!(ordinary.ok, "{ordinary:?}");
        assert_eq!(ordinary.result.unwrap()["blocking"], 1);
        assert!(started.elapsed() >= Duration::from_millis(140));
        assert_eq!(fs::read_to_string(&action_log).unwrap(), "history\nno_action\n");
        assert!(
            fs::read_to_string(&history_log)
                .unwrap()
                .contains("Initial specification")
        );

        let stop_started = Instant::now();
        let stop = gateway
            .dispatch(native_request(
                "ordinary-stop",
                json!({
                    "hook_event_name":"Stop", "session_id":"thread-a",
                    "turn_id":"turn-thread-a", "cwd":temp.path(),
                    "last_assistant_message":"Implementation status only"
                }),
            ))
            .await;
        assert!(stop.ok, "{stop:?}");
        assert_eq!(stop.result.unwrap()["blocking"], 1);
        assert!(stop_started.elapsed() >= Duration::from_millis(140));

        let decision = gateway
            .dispatch(native_request(
                "decision-post-tool",
                json!({
                    "hook_event_name":"PostToolUse", "session_id":"thread-a",
                    "turn_id":"turn-thread-a", "cwd":temp.path(), "tool_use_id":"tool-2",
                    "tool_name":"shell", "tool_response":{"requires_decision":true}
                }),
            ))
            .await;
        assert!(decision.ok, "{decision:?}");
        assert_eq!(decision.result.unwrap()["blocking"], 1);

        server_for_run
            .emit_notification(
                "turn/started",
                json!({"threadId":"thread-b","turn":{"id":"turn-thread-b","status":"inProgress","items":[{
                    "id":"initial-b","type":"userMessage","content":[{"type":"text","text":"A separate implementation task."}]
                }]}}),
            )
            .await;
        let activation_b = gateway
            .dispatch(native_request(
                "activate-b",
                json!({
                    "hook_event_name":"UserPromptSubmit", "session_id":"thread-b",
                    "turn_id":"turn-thread-b", "cwd":temp.path(),
                    "prompt":format!("[$unspecified-decisions]({}) review", marker.display())
                }),
            ))
            .await;
        assert!(activation_b.ok, "{activation_b:?}");
        let missing = gateway
            .dispatch(native_request(
                "missing-baseline",
                json!({
                    "hook_event_name":"PostToolUse", "session_id":"thread-b",
                    "turn_id":"turn-thread-b", "cwd":temp.path(), "tool_use_id":"tool-b",
                    "tool_name":"shell", "tool_response":{"missing_baseline":true}
                }),
            ))
            .await;
        assert!(missing.ok, "{missing:?}");

        let actions = fs::read_to_string(&action_log).unwrap();
        assert_eq!(
            actions.lines().collect::<Vec<_>>(),
            [
                "history",
                "no_action",
                "no_action",
                "steer",
                "interrupt",
                "history",
                "steer",
                "interrupt",
            ]
        );
        let args = fs::read_to_string(&args_log).unwrap();
        let invocations = args.lines().collect::<Vec<_>>();
        assert_eq!(invocations.len(), 4);
        assert!(invocations.iter().all(|args| args.contains("--model sonnet")));
        assert!(invocations[0].contains("--session-id"));
        assert!(invocations[1].contains("--resume"));
        assert!(invocations[2].contains("--resume"));
        assert!(invocations[3].contains("--session-id"));

        let requests = server_for_run.received().await;
        let control_methods = requests
            .iter()
            .filter_map(|request| request["method"].as_str())
            .filter(|method| matches!(*method, "turn/steer" | "turn/interrupt"))
            .collect::<Vec<_>>();
        assert_eq!(
            control_methods,
            ["turn/steer", "turn/interrupt", "turn/steer", "turn/interrupt"]
        );
        assert!(
            requests.iter().any(|request| {
                request["method"] == "turn/steer"
                    && request["params"].to_string().contains("Which architecture")
            }),
            "the stop explanation and one question must reach Codex"
        );
        assert!(
            !BRIDGE_EVENTS.contains(&"PostToolUseFailure"),
            "failed-tool events are observer-only and cannot claim a native barrier"
        );
        assert!(
            fs::read_to_string(&input_log)
                .unwrap()
                .contains("gap` shows that the")
        );

        let _ = shutdown_tx.send(true);
        serving.await.unwrap().unwrap();
        sessions.shutdown().await.unwrap();
    })
    .await
    .unwrap();
    server.shutdown().await;
}
