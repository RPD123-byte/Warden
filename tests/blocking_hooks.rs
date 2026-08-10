use codex_control::{CodexControl, Config as ControlConfig};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::{io::AsyncWriteExt, net::UnixStream, sync::watch};
use transport::mock::MockAppServer;
use warden_daemon::{
    Config, DataPaths, HookRegistry,
    action::{ACTION_PROTOCOL_VERSION, ActionGateway, GatewayRequest, NoAgentBackend},
    activation::ActivationRouter,
    python::PythonRuntime,
};

fn control_config(socket: &Path) -> ControlConfig {
    let mut config = ControlConfig {
        manage_gui: false,
        ..ControlConfig::default()
    };
    config.transport.socket_path = socket.to_owned();
    config.transport.connect_timeout = Duration::from_millis(300);
    config.transport.request_timeout = Duration::from_secs(1);
    config
}

fn hook_source(blocking: bool, output: &Path, delay: f64) -> String {
    let output = serde_json::to_string(output.to_str().unwrap()).unwrap();
    format!(
        "import time\nfrom pathlib import Path\nfrom warden import hook, HookEventKind\n@hook(on=[HookEventKind.USER_PROMPT_SUBMITTED], blocking={blocking})\ndef run(event):\n    time.sleep({delay})\n    Path({output}).write_text('finished', encoding='utf-8')\n",
        blocking = if blocking { "True" } else { "False" },
    )
}

fn tool_hook_source(blocking: bool, output: &Path, delay: f64) -> String {
    let output = serde_json::to_string(output.to_str().unwrap()).unwrap();
    format!(
        "import time\nfrom pathlib import Path\nfrom warden import hook, HookEventKind\n@hook(on=[HookEventKind.PRE_TOOL_USE, HookEventKind.POST_TOOL_USE], blocking={blocking})\ndef run(event):\n    time.sleep({delay})\n    with open({output}, 'a', encoding='utf-8') as stream:\n        stream.write(event.kind.value + '\\n')\n",
        blocking = if blocking { "True" } else { "False" },
    )
}

fn native_request(marker: &Path, token: &str, id: &str) -> GatewayRequest {
    let name = marker
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    GatewayRequest {
        message_type: "request".into(),
        protocol_version: ACTION_PROTOCOL_VERSION,
        id: id.into(),
        method: "warden.native_hook.event".into(),
        params: json!({
            "session_id": "thread",
            "turn_id": id,
            "cwd": "/tmp",
            "hook_event_name": "UserPromptSubmit",
            "prompt": format!("[${name}]({}) run", marker.display()),
        }),
        context: None,
        bridge_auth: Some(token.into()),
    }
}

fn bridge_request(id: &str, token: &str, params: Value) -> GatewayRequest {
    GatewayRequest {
        message_type: "request".into(),
        protocol_version: ACTION_PROTOCOL_VERSION,
        id: id.into(),
        method: "warden.native_hook.event".into(),
        params,
        context: None,
        bridge_auth: Some(token.into()),
    }
}

#[tokio::test]
async fn native_barrier_waits_for_blocking_and_releases_non_blocking() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    let paths = DataPaths::under(temp.path().join("warden"));
    let mut config = Config {
        paths: paths.clone(),
        codex_home: temp.path().join("codex"),
        agents_home: temp.path().join("agents"),
        hook_timeout: Duration::from_secs(3),
        max_concurrent_hooks: 2,
        ..Config::default()
    };
    config.python_sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
    paths.create_all().unwrap();

    CodexControl::run(control_config(&rpc_socket), move |handle| async move {
        let hook_dir = paths.hooks.join("timing");
        fs::create_dir_all(&hook_dir).unwrap();
        let output = temp.path().join("blocking-finished");
        fs::write(hook_dir.join("hook.py"), hook_source(true, &output, 0.25)).unwrap();
        let python = Arc::new(PythonRuntime::new(&config));
        let registry = HookRegistry::new(
            paths.hooks.clone(),
            paths.modules.clone(),
            paths.generated_skills.clone(),
            paths.runtimes.clone(),
            python.clone(),
        );
        registry.refresh().await.unwrap();
        assert!(
            registry
                .current(&warden_daemon::HookId::parse("timing").unwrap())
                .await
                .unwrap()
                .metadata
                .blocking
        );
        let marker = paths.generated_skills.join("timing/SKILL.md");
        let router = ActivationRouter::new(paths.generated_skills.clone(), registry.clone());
        let gateway = ActionGateway::new(
            handle,
            paths.action_socket.clone(),
            Arc::new(NoAgentBackend),
        )
        .with_native_hook_runtime(
            router.clone(),
            python.clone(),
            "bridge-secret".into(),
            2,
        );

        let started = Instant::now();
        let response = gateway
            .dispatch(native_request(&marker, "bridge-secret", "blocking"))
            .await;
        assert!(response.ok, "{response:?}");
        assert_eq!(
            response.result.as_ref().unwrap()["blocking"],
            1,
            "{response:?}"
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(fs::read_to_string(&output).unwrap(), "finished");

        let output = temp.path().join("non-blocking-finished");
        fs::write(hook_dir.join("hook.py"), hook_source(false, &output, 0.30)).unwrap();
        registry.refresh().await.unwrap();
        let started = Instant::now();
        let response = gateway
            .dispatch(native_request(&marker, "bridge-secret", "nonblocking"))
            .await;
        assert!(response.ok, "{response:?}");
        assert!(started.elapsed() < Duration::from_millis(150));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !output.is_file() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn native_bundle_maps_all_events_authenticates_and_scopes_activation_to_one_turn() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    let paths = DataPaths::under(temp.path().join("warden"));
    let output = temp.path().join("events.jsonl");
    let mut config = Config {
        paths: paths.clone(),
        codex_home: temp.path().join("codex"),
        agents_home: temp.path().join("agents"),
        hook_timeout: Duration::from_secs(3),
        max_concurrent_hooks: 4,
        ..Config::default()
    };
    config.python_sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
    paths.create_all().unwrap();

    CodexControl::run(control_config(&rpc_socket), move |handle| async move {
        let hook_dir = paths.hooks.join("mapping");
        fs::create_dir_all(&hook_dir).unwrap();
        let output_literal = serde_json::to_string(output.to_str().unwrap()).unwrap();
        fs::write(
            hook_dir.join("hook.py"),
            format!(
                "import json\nfrom warden import hook, HookEventKind\nEVENTS = [HookEventKind.USER_PROMPT_SUBMITTED, HookEventKind.TURN_STARTED, HookEventKind.PRE_TOOL_USE, HookEventKind.POST_TOOL_USE, HookEventKind.AGENT_MESSAGE_COMPLETED]\n@hook(on=EVENTS, blocking=True)\ndef run(event):\n    with open({output_literal}, 'a', encoding='utf-8') as stream:\n        stream.write(json.dumps(event.to_dict(), separators=(',', ':')) + '\\n')\n"
            ),
        )
        .unwrap();
        let python = Arc::new(PythonRuntime::new(&config));
        let registry = HookRegistry::new(
            paths.hooks.clone(),
            paths.modules.clone(),
            paths.generated_skills.clone(),
            paths.runtimes.clone(),
            python.clone(),
        );
        registry.refresh().await.unwrap();
        let marker = paths.generated_skills.join("mapping/SKILL.md");
        let gateway = ActionGateway::new(
            handle,
            paths.action_socket.clone(),
            Arc::new(NoAgentBackend),
        )
        .with_native_hook_runtime(
            ActivationRouter::new(paths.generated_skills.clone(), registry),
            python,
            "bridge-secret".into(),
            4,
        );

        let denied = gateway
            .dispatch(bridge_request("denied", "wrong", Value::Null))
            .await;
        assert!(!denied.ok);
        assert_eq!(denied.error.unwrap().code, "unauthorized");

        let prompt = gateway
            .dispatch(bridge_request(
                "prompt",
                "bridge-secret",
                json!({
                    "hook_event_name": "UserPromptSubmit",
                    "session_id": "thread",
                    "turn_id": "turn",
                    "prompt": format!("[$mapping]({}) run", marker.display())
                }),
            ))
            .await;
        assert!(prompt.ok, "{prompt:?}");
        assert_eq!(prompt.result.unwrap()["blocking"], 2);

        for (id, params) in [
            (
                "pre",
                json!({
                    "hook_event_name": "PreToolUse", "session_id": "thread",
                    "turn_id": "turn", "tool_use_id": "tool", "tool_name": "shell",
                    "tool_input": {"command": "true"}
                }),
            ),
            (
                "post",
                json!({
                    "hook_event_name": "PostToolUse", "session_id": "thread",
                    "turn_id": "turn", "tool_use_id": "tool", "tool_name": "shell",
                    "tool_response": {"ok": true}
                }),
            ),
            (
                "stop",
                json!({
                    "hook_event_name": "Stop", "session_id": "thread",
                    "turn_id": "turn", "last_assistant_message": "done"
                }),
            ),
        ] {
            let response = gateway
                .dispatch(bridge_request(id, "bridge-secret", params))
                .await;
            assert!(response.ok, "{response:?}");
            assert_eq!(response.result.unwrap()["blocking"], 1);
        }

        let incomplete_stop = gateway
            .dispatch(bridge_request(
                "stop-without-message",
                "bridge-secret",
                json!({
                    "hook_event_name": "Stop", "session_id": "thread", "turn_id": "turn"
                }),
            ))
            .await;
        assert!(incomplete_stop.ok, "{incomplete_stop:?}");
        assert_eq!(incomplete_stop.result.unwrap()["blocking"], 0);

        let next = gateway
            .dispatch(bridge_request(
                "next",
                "bridge-secret",
                json!({
                    "hook_event_name": "UserPromptSubmit", "session_id": "thread",
                    "turn_id": "next", "prompt": "plain message"
                }),
            ))
            .await;
        assert!(next.ok, "{next:?}");
        assert_eq!(next.result.unwrap()["blocking"], 0);

        let events = fs::read_to_string(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 5);
        assert_eq!(events[0]["kind"], "user_prompt_submitted");
        assert_eq!(events[1]["kind"], "turn_started");
        assert_eq!(events[2]["kind"], "pre_tool_use");
        assert_eq!(events[3]["kind"], "post_tool_use");
        assert_eq!(events[4]["kind"], "agent_message_completed");
        assert!(events.iter().all(|event| event["origin"] == "native"));
        assert!(events.iter().all(|event| event["source_sequence"].is_null()));
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn mixed_modes_run_blocking_hooks_concurrently_without_waiting_for_non_blocking() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    let paths = DataPaths::under(temp.path().join("warden"));
    let mut config = Config {
        paths: paths.clone(),
        codex_home: temp.path().join("codex"),
        agents_home: temp.path().join("agents"),
        hook_timeout: Duration::from_secs(3),
        max_concurrent_hooks: 4,
        ..Config::default()
    };
    config.python_sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
    paths.create_all().unwrap();

    CodexControl::run(control_config(&rpc_socket), move |handle| async move {
        for (name, blocking, delay) in [
            ("first", true, 0.4),
            ("second", true, 0.4),
            ("background", false, 0.8),
        ] {
            let directory = paths.hooks.join(name);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("hook.py"),
                hook_source(blocking, &temp.path().join(format!("{name}.done")), delay),
            )
            .unwrap();
        }
        let python = Arc::new(PythonRuntime::new(&config));
        let registry = HookRegistry::new(
            paths.hooks.clone(),
            paths.modules.clone(),
            paths.generated_skills.clone(),
            paths.runtimes.clone(),
            python.clone(),
        );
        registry.refresh().await.unwrap();
        let prompt = ["first", "second", "background"]
            .into_iter()
            .map(|name| {
                format!(
                    "[${name}]({})",
                    paths.generated_skills.join(name).join("SKILL.md").display()
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let gateway = ActionGateway::new(
            handle,
            paths.action_socket.clone(),
            Arc::new(NoAgentBackend),
        )
        .with_native_hook_runtime(
            ActivationRouter::new(paths.generated_skills.clone(), registry),
            python,
            "bridge-secret".into(),
            4,
        );

        let started = Instant::now();
        let response = gateway
            .dispatch(bridge_request(
                "mixed",
                "bridge-secret",
                json!({
                    "hook_event_name": "UserPromptSubmit", "session_id": "thread",
                    "turn_id": "mixed", "prompt": format!("{prompt} run")
                }),
            ))
            .await;
        let elapsed = started.elapsed();
        assert!(response.ok, "{response:?}");
        let result = response.result.unwrap();
        assert_eq!(result["blocking"], 2);
        assert_eq!(result["non_blocking"], 1);
        assert!(
            elapsed >= Duration::from_millis(350),
            "released too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(700),
            "blocking hooks ran serially: {elapsed:?}"
        );
        assert!(!temp.path().join("background.done").is_file());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !temp.path().join("background.done").is_file() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn non_blocking_queue_rejects_saturation_and_drains_with_bounded_capacity() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    let paths = DataPaths::under(temp.path().join("warden"));
    let mut config = Config {
        paths: paths.clone(),
        codex_home: temp.path().join("codex"),
        agents_home: temp.path().join("agents"),
        hook_timeout: Duration::from_secs(3),
        max_concurrent_hooks: 1,
        ..Config::default()
    };
    config.python_sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
    paths.create_all().unwrap();

    CodexControl::run(control_config(&rpc_socket), move |handle| async move {
        let hook_dir = paths.hooks.join("slow");
        fs::create_dir_all(&hook_dir).unwrap();
        fs::write(
            hook_dir.join("hook.py"),
            hook_source(false, &temp.path().join("slow.done"), 0.3),
        )
        .unwrap();
        let python = Arc::new(PythonRuntime::new(&config));
        let registry = HookRegistry::new(
            paths.hooks.clone(),
            paths.modules.clone(),
            paths.generated_skills.clone(),
            paths.runtimes.clone(),
            python.clone(),
        );
        registry.refresh().await.unwrap();
        let marker = paths.generated_skills.join("slow/SKILL.md");
        let gateway = ActionGateway::new(
            handle,
            paths.action_socket.clone(),
            Arc::new(NoAgentBackend),
        )
        .with_native_hook_runtime(
            ActivationRouter::new(paths.generated_skills.clone(), registry),
            python,
            "bridge-secret".into(),
            1,
        );

        let mut accepted = 0_u64;
        let mut rejected = 0_u64;
        for index in 0..10 {
            let response = gateway
                .dispatch(bridge_request(
                    &format!("load-{index}"),
                    "bridge-secret",
                    json!({
                        "hook_event_name": "UserPromptSubmit", "session_id": "thread",
                        "turn_id": format!("load-{index}"),
                        "prompt": format!("[$slow]({}) run", marker.display())
                    }),
                ))
                .await;
            assert!(response.ok, "{response:?}");
            let result = response.result.unwrap();
            accepted += result["non_blocking"].as_u64().unwrap();
            rejected += result["rejected_non_blocking"].as_u64().unwrap();
        }
        assert_eq!(accepted, 4);
        assert_eq!(rejected, 6);

        tokio::time::sleep(Duration::from_millis(1400)).await;
        let health = gateway
            .dispatch(GatewayRequest {
                message_type: "request".into(),
                protocol_version: ACTION_PROTOCOL_VERSION,
                id: "health".into(),
                method: "warden.health".into(),
                params: json!({}),
                context: None,
                bridge_auth: None,
            })
            .await;
        let dispatcher = &health.result.unwrap()["dispatcher"];
        assert_eq!(dispatcher["non_blocking_queue_limit"], 4);
        assert_eq!(dispatcher["non_blocking_queued"], 0);
        assert_eq!(dispatcher["non_blocking_active"], 0);
        assert_eq!(dispatcher["non_blocking_rejected"], 6);
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn pre_and_post_tool_boundaries_wait_only_for_blocking_revisions() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    let paths = DataPaths::under(temp.path().join("warden"));
    let output = temp.path().join("tool-events");
    let mut config = Config {
        paths: paths.clone(),
        codex_home: temp.path().join("codex"),
        agents_home: temp.path().join("agents"),
        hook_timeout: Duration::from_secs(3),
        max_concurrent_hooks: 2,
        ..Config::default()
    };
    config.python_sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
    paths.create_all().unwrap();

    CodexControl::run(control_config(&rpc_socket), move |handle| async move {
        let hook_dir = paths.hooks.join("tools");
        fs::create_dir_all(&hook_dir).unwrap();
        fs::write(
            hook_dir.join("hook.py"),
            tool_hook_source(true, &output, 0.25),
        )
        .unwrap();
        let python = Arc::new(PythonRuntime::new(&config));
        let registry = HookRegistry::new(
            paths.hooks.clone(),
            paths.modules.clone(),
            paths.generated_skills.clone(),
            paths.runtimes.clone(),
            python.clone(),
        );
        registry.refresh().await.unwrap();
        let marker = paths.generated_skills.join("tools/SKILL.md");
        let gateway = ActionGateway::new(
            handle,
            paths.action_socket.clone(),
            Arc::new(NoAgentBackend),
        )
        .with_native_hook_runtime(
            ActivationRouter::new(paths.generated_skills.clone(), registry.clone()),
            python,
            "bridge-secret".into(),
            2,
        );

        for (turn, blocking) in [("blocking-tools", true), ("background-tools", false)] {
            if !blocking {
                fs::write(
                    hook_dir.join("hook.py"),
                    tool_hook_source(false, &output, 0.25),
                )
                .unwrap();
                registry.refresh().await.unwrap();
            }
            let activation = gateway
                .dispatch(bridge_request(
                    &format!("activate-{turn}"),
                    "bridge-secret",
                    json!({
                        "hook_event_name": "UserPromptSubmit", "session_id": "thread",
                        "turn_id": turn,
                        "prompt": format!("[$tools]({}) run", marker.display())
                    }),
                ))
                .await;
            assert!(activation.ok, "{activation:?}");
            for (event_name, id) in [("PreToolUse", "pre"), ("PostToolUse", "post")] {
                let started = Instant::now();
                let response = gateway
                    .dispatch(bridge_request(
                        &format!("{id}-{turn}"),
                        "bridge-secret",
                        json!({
                            "hook_event_name": event_name, "session_id": "thread",
                            "turn_id": turn, "tool_use_id": format!("tool-{id}")
                        }),
                    ))
                    .await;
                assert!(response.ok, "{response:?}");
                if blocking {
                    assert!(started.elapsed() >= Duration::from_millis(200));
                    assert_eq!(response.result.unwrap()["blocking"], 1);
                } else {
                    assert!(started.elapsed() < Duration::from_millis(150));
                    assert_eq!(response.result.unwrap()["non_blocking"], 1);
                }
            }
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while fs::read_to_string(&output)
                .map(|value| value.lines().count())
                .unwrap_or(0)
                < 4
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn disconnect_does_not_cancel_native_blocking_hook_and_replacement_stays_healthy() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    let paths = DataPaths::under(temp.path().join("warden"));
    let output = temp.path().join("cancelled-then-healthy");
    let mut config = Config {
        paths: paths.clone(),
        codex_home: temp.path().join("codex"),
        agents_home: temp.path().join("agents"),
        hook_timeout: Duration::from_secs(3),
        max_concurrent_hooks: 1,
        ..Config::default()
    };
    config.python_sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
    paths.create_all().unwrap();

    CodexControl::run(control_config(&rpc_socket), move |handle| async move {
        let hook_dir = paths.hooks.join("cancel");
        fs::create_dir_all(&hook_dir).unwrap();
        fs::write(hook_dir.join("hook.py"), hook_source(true, &output, 0.5)).unwrap();
        let python = Arc::new(PythonRuntime::new(&config));
        let registry = HookRegistry::new(
            paths.hooks.clone(),
            paths.modules.clone(),
            paths.generated_skills.clone(),
            paths.runtimes.clone(),
            python.clone(),
        );
        registry.refresh().await.unwrap();
        let marker = paths.generated_skills.join("cancel/SKILL.md");
        let gateway = ActionGateway::new(
            handle,
            paths.action_socket.clone(),
            Arc::new(NoAgentBackend),
        )
        .with_native_hook_runtime(
            ActivationRouter::new(paths.generated_skills.clone(), registry),
            python,
            "bridge-secret".into(),
            1,
        );
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let server_gateway = gateway.clone();
        let socket_task = tokio::spawn(async move {
            server_gateway
                .serve(shutdown_rx, 1024 * 1024)
                .await
                .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !paths.action_socket.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let request = native_request(&marker, "bridge-secret", "cancelled");
        let mut encoded = serde_json::to_vec(&request).unwrap();
        encoded.push(b'\n');
        let mut stream = UnixStream::connect(&paths.action_socket).await.unwrap();
        stream.write_all(&encoded).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let health = gateway
                    .dispatch(GatewayRequest {
                        message_type: "request".into(),
                        protocol_version: ACTION_PROTOCOL_VERSION,
                        id: "health-during-cancel".into(),
                        method: "warden.health".into(),
                        params: json!({}),
                        context: None,
                        bridge_auth: None,
                    })
                    .await;
                if health.result.unwrap()["active_invocations"] == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(stream);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let health = gateway
                    .dispatch(GatewayRequest {
                        message_type: "request".into(),
                        protocol_version: ACTION_PROTOCOL_VERSION,
                        id: "health-after-disconnect".into(),
                        method: "warden.health".into(),
                        params: json!({}),
                        context: None,
                        bridge_auth: None,
                    })
                    .await;
                if health.result.unwrap()["active_invocations"] == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("native hook must finish after its bridge client disconnects");
        assert_eq!(fs::read_to_string(&output).unwrap(), "finished");

        let replacement = gateway
            .dispatch(native_request(&marker, "bridge-secret", "replacement"))
            .await;
        assert!(replacement.ok, "{replacement:?}");
        assert_eq!(replacement.result.unwrap()["blocking"], 1);
        assert_eq!(fs::read_to_string(&output).unwrap(), "finished");

        let _ = shutdown_tx.send(true);
        socket_task.await.unwrap();
    })
    .await
    .unwrap();
    server.shutdown().await;
}
