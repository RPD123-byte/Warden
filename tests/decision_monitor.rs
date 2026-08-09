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
use tokio::{io::AsyncWriteExt, net::UnixStream, sync::watch};
use transport::mock::{Fault, MockAppServer, MockThread};
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
    printf 'interrupt\n' >> "$ACTION_LOG"
    warden action turn_interrupt --arguments '{}' >> "$ACTION_RESULT_LOG"
    sleep 0.20
    printf 'start\n' >> "$ACTION_LOG"
    warden action turn_start --arguments '{"input":[{"type":"text","text":"The Warden supervisor stopped the previous implementation turn. Present this notice and question without resuming implementation: Stopped because architecture was unspecified. Which architecture should Codex use?"}]}' >> "$ACTION_RESULT_LOG"
    ;;
  *missing_baseline*)
    printf 'interrupt\n' >> "$ACTION_LOG"
    warden action turn_interrupt --arguments '{}' >> "$ACTION_RESULT_LOG"
    printf 'start\n' >> "$ACTION_LOG"
    warden action turn_start --arguments '{"input":[{"type":"text","text":"The Warden supervisor stopped the previous implementation turn. Present this notice and question without resuming implementation: The initial review baseline is unavailable. Which request or specification governs this implementation?"}]}' >> "$ACTION_RESULT_LOG"
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

fn health_request(id: &str) -> GatewayRequest {
    GatewayRequest {
        message_type: "request".into(),
        protocol_version: ACTION_PROTOCOL_VERSION,
        id: id.into(),
        method: "warden.health".into(),
        params: json!({}),
        context: None,
        bridge_auth: None,
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
async fn template_blocks_reuses_context_and_delivers_question_after_interrupt() {
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
        let marker = paths.generated_skills.join("unspecified-decisions/SKILL.md");
        let start_marker = paths
            .generated_skills
            .join("unspecified-decisions-start/SKILL.md");
        let pause_marker = paths
            .generated_skills
            .join("unspecified-decisions-pause/SKILL.md");
        let resume_marker = paths
            .generated_skills
            .join("unspecified-decisions-resume/SKILL.md");
        let stop_marker = paths
            .generated_skills
            .join("unspecified-decisions-stop/SKILL.md");
        for path in [&start_marker, &pause_marker, &resume_marker, &stop_marker] {
            assert!(path.is_file(), "missing stateful control marker {}", path.display());
        }
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
                    "prompt":format!("[$unspecified-decisions-start]({}) review continuously", start_marker.display())
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

        let pause = gateway
            .dispatch(native_request(
                "pause-a",
                json!({
                    "hook_event_name":"UserPromptSubmit", "session_id":"thread-a",
                    "turn_id":"pause-thread-a", "cwd":temp.path(),
                    "prompt":format!("[$unspecified-decisions-pause]({}) pause", pause_marker.display())
                }),
            ))
            .await;
        assert!(pause.ok, "{pause:?}");
        let paused_health = gateway.dispatch(health_request("paused-health")).await;
        assert_eq!(
            paused_health.result.unwrap()["daemon"]["continuous_sessions"]["paused"],
            1
        );
        let paused = gateway
            .dispatch(native_request(
                "paused-post-tool",
                json!({
                    "hook_event_name":"PostToolUse", "session_id":"thread-a",
                    "turn_id":"pause-thread-a", "cwd":temp.path(), "tool_use_id":"paused-tool",
                    "tool_name":"shell", "tool_response":{"ok":true}
                }),
            ))
            .await;
        assert!(paused.ok, "{paused:?}");
        assert_eq!(paused.result.unwrap()["blocking"], 0);
        assert_eq!(fs::read_to_string(&args_log).unwrap().lines().count(), 1);

        let resume = gateway
            .dispatch(native_request(
                "resume-a",
                json!({
                    "hook_event_name":"UserPromptSubmit", "session_id":"thread-a",
                    "turn_id":"resume-thread-a", "cwd":temp.path(),
                    "prompt":format!("[$unspecified-decisions-resume]({}) resume", resume_marker.display())
                }),
            ))
            .await;
        assert!(resume.ok, "{resume:?}");
        let resumed = gateway
            .dispatch(native_request(
                "resumed-post-tool",
                json!({
                    "hook_event_name":"PostToolUse", "session_id":"thread-a",
                    "turn_id":"resume-thread-a", "cwd":temp.path(), "tool_use_id":"resumed-tool",
                    "tool_name":"shell", "tool_response":{"ok":true}
                }),
            ))
            .await;
        assert!(resumed.ok, "{resumed:?}");
        assert_eq!(resumed.result.unwrap()["blocking"], 1);

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

        server_for_run
            .set_fault(Fault::InterruptCompletionBeforeResponse)
            .await;
        let decision = native_request(
            "decision-post-tool",
            json!({
                "hook_event_name":"PostToolUse", "session_id":"thread-a",
                "turn_id":"turn-thread-a", "cwd":temp.path(), "tool_use_id":"tool-2",
                "tool_name":"shell", "tool_response":{"requires_decision":true}
            }),
        );
        let mut encoded = serde_json::to_vec(&decision).unwrap();
        encoded.push(b'\n');
        let mut bridge = UnixStream::connect(&paths.action_socket).await.unwrap();
        bridge.write_all(&encoded).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if fs::read_to_string(&action_result_log)
                    .is_ok_and(|results| !results.trim().is_empty())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "interrupt action did not finish; actions={:?}; inputs={:?}",
                fs::read_to_string(&action_log),
                fs::read_to_string(&input_log)
            )
        });
        drop(bridge);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if fs::read_to_string(&action_log)
                    .is_ok_and(|actions| actions.lines().any(|action| action == "start"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("turn-start must run after interruption disconnects the native bridge");

        let stopped = gateway
            .dispatch(native_request(
                "stop-continuous-a",
                json!({
                    "hook_event_name":"UserPromptSubmit", "session_id":"thread-a",
                    "turn_id":"stop-continuous-thread-a", "cwd":temp.path(),
                    "prompt":format!("[$unspecified-decisions-stop]({}) stop", stop_marker.display())
                }),
            ))
            .await;
        assert!(stopped.ok, "{stopped:?}");
        let stopped_health = gateway.dispatch(health_request("stopped-health")).await;
        assert_eq!(
            stopped_health.result.unwrap()["daemon"]["continuous_sessions"]["running"],
            0
        );
        let after_stop = gateway
            .dispatch(native_request(
                "after-stop-a",
                json!({
                    "hook_event_name":"PostToolUse", "session_id":"thread-a",
                    "turn_id":"stop-continuous-thread-a", "cwd":temp.path(), "tool_use_id":"after-stop-tool",
                    "tool_name":"shell", "tool_response":{"ok":true}
                }),
            ))
            .await;
        assert!(after_stop.ok, "{after_stop:?}");
        assert_eq!(after_stop.result.unwrap()["blocking"], 0);

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
                "no_action",
                "interrupt",
                "start",
                "history",
                "interrupt",
                "start",
            ]
        );
        let args = fs::read_to_string(&args_log).unwrap();
        let invocations = args.lines().collect::<Vec<_>>();
        assert_eq!(invocations.len(), 5);
        assert!(invocations.iter().all(|args| args.contains("--model sonnet")));
        assert!(invocations[0].contains("--session-id"));
        assert!(invocations[1].contains("--resume"));
        assert!(invocations[2].contains("--resume"));
        assert!(invocations[3].contains("--resume"));
        assert!(invocations[4].contains("--session-id"));

        let requests = server_for_run.received().await;
        let control_methods = requests
            .iter()
            .filter_map(|request| request["method"].as_str())
            .filter(|method| matches!(*method, "turn/interrupt" | "turn/start"))
            .collect::<Vec<_>>();
        assert_eq!(
            control_methods,
            ["turn/interrupt", "turn/start", "turn/interrupt", "turn/start"]
        );
        assert!(
            requests.iter().any(|request| {
                request["method"] == "turn/start"
                    && request["params"].to_string().contains("Which architecture")
            }),
            "a fresh Codex turn must durably receive the stop explanation and one question"
        );
        assert!(
            !BRIDGE_EVENTS.contains(&"PostToolUseFailure"),
            "failed-tool events are observer-only and cannot claim a native barrier"
        );
        let prompts = fs::read_to_string(&input_log).unwrap();
        assert!(prompts.contains("only user-authored instructions and governing specifications"));
        assert!(prompts.contains("Silence and a previous no-action verdict are not approval"));
        assert!(prompts.contains("baseline is unavailable"));

        let _ = shutdown_tx.send(true);
        serving.await.unwrap().unwrap();
        sessions.shutdown().await.unwrap();
    })
    .await
    .unwrap();
    server.shutdown().await;
}
