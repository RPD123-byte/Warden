use codex_control::{CodexControl, Config as ControlConfig};
use serde_json::json;
use std::{fs, path::Path, time::Duration};
use tempfile::TempDir;
use tokio::sync::oneshot;
use transport::mock::{MockAppServer, MockThread};
use warden_daemon::{Config, DataPaths, MARKER_BODY, Warden};

fn control_config(socket: &Path) -> ControlConfig {
    let mut config = ControlConfig {
        manage_gui: false,
        ..ControlConfig::default()
    };
    config.transport.socket_path = socket.to_owned();
    config.transport.connect_timeout = Duration::from_millis(300);
    config.transport.request_timeout = Duration::from_secs(1);
    config.transport.retry_initial = Duration::from_millis(20);
    config.transport.retry_max = Duration::from_millis(50);
    config
}

async fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(timeout, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition did not become true");
}

fn line_count(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

async fn daemon_health(socket: &Path) -> serde_json::Value {
    let health = tokio::process::Command::new(env!("CARGO_BIN_EXE_warden"))
        .args(["--socket", socket.to_str().unwrap(), "health"])
        .output()
        .await
        .unwrap();
    assert!(
        health.status.success(),
        "{}",
        String::from_utf8_lossy(&health.stderr)
    );
    serde_json::from_slice(&health.stdout).unwrap()
}

#[tokio::test]
async fn hot_created_marker_routes_one_selected_turn_and_not_the_next() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    let active_task_cwd = temp.path().join("active-task");
    let preexisting_idle_task_cwd = temp.path().join("preexisting-idle-task");
    fs::create_dir_all(&active_task_cwd).unwrap();
    fs::create_dir_all(&preexisting_idle_task_cwd).unwrap();
    server
        .add_thread(MockThread {
            id: "thread".into(),
            cwd: active_task_cwd.clone(),
            status: "active".into(),
            turn_id: Some("preexisting-turn".into()),
            ephemeral: false,
            updated_at: 2,
        })
        .await;
    server
        .add_thread(MockThread {
            id: "idle-thread".into(),
            cwd: preexisting_idle_task_cwd.clone(),
            status: "idle".into(),
            turn_id: None,
            ephemeral: false,
            updated_at: 1,
        })
        .await;
    let mut warden_config = Config {
        paths: DataPaths::under(temp.path().join("warden")),
        codex_home: temp.path().join("codex-home"),
        hook_timeout: Duration::from_secs(3),
        max_hook_message_bytes: 256 * 1024,
        manage_gui: false,
        ..Config::default()
    };
    // Keep the test independent of the process working directory.
    warden_config.python_sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
    let output = temp.path().join("received.jsonl");
    let listed_threads_output = temp.path().join("listed-threads.json");
    let server_for_run = server.clone();
    let active_task_cwd_for_run = active_task_cwd.clone();
    let preexisting_idle_task_cwd_for_run = preexisting_idle_task_cwd.clone();
    CodexControl::run(control_config(&rpc_socket), move |handle| async move {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if handle
                    .snapshot()
                    .await
                    .threads
                    .get("thread")
                    .is_some_and(|thread| thread.subscribed)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("pre-existing task was not subscribed by codex-control");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = handle.snapshot().await;
                let observed = snapshot
                    .threads
                    .get("thread")
                    .and_then(|thread| thread.raw_thread.as_ref())
                    .and_then(|thread| thread.get("cwd"))
                    .and_then(serde_json::Value::as_str)
                    == active_task_cwd_for_run.to_str();
                if observed {
                    assert!(
                        !snapshot.threads.contains_key("idle-thread"),
                        "the idle task must be absent from the reducer so this test exercises the typed thread-list API"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("active task CWD was not retained by codex-control");

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let paths = warden_config.paths.clone();
        let runtime = tokio::spawn(Warden::serve_until(
            warden_config,
            handle,
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        wait_until(Duration::from_secs(3), || paths.action_socket.exists()).await;
        let hook_dir = paths.hooks.join("record-events");
        fs::create_dir_all(&hook_dir).unwrap();
        let output_literal = serde_json::to_string(output.to_str().unwrap()).unwrap();
        let listed_threads_literal =
            serde_json::to_string(listed_threads_output.to_str().unwrap()).unwrap();
        fs::write(
            hook_dir.join("hook.py"),
            format!(
                "import json\nfrom warden import hook, HookEventKind, WardenAction\nEVENTS = [HookEventKind.USER_PROMPT_SUBMITTED, HookEventKind.TURN_STARTED, HookEventKind.PRE_TOOL_USE, HookEventKind.POST_TOOL_USE, HookEventKind.AGENT_MESSAGE_COMPLETED, HookEventKind.TURN_COMPLETED]\n@hook(on=EVENTS, actions=[WardenAction.THREAD_LIST])\nasync def run(event, warden):\n    if event.kind == HookEventKind.USER_PROMPT_SUBMITTED:\n        with open({listed_threads_literal}, 'w', encoding='utf-8') as stream:\n            json.dump(await warden.list_threads(), stream, separators=(',', ':'))\n    with open({output_literal}, 'a', encoding='utf-8') as stream:\n        stream.write(json.dumps(event.to_dict(), separators=(',', ':')) + '\\n')\n"
            ),
        )
        .unwrap();

        let marker = paths
            .generated_skills
            .join("record-events/SKILL.md");
        wait_until(Duration::from_secs(20), || marker.is_file()).await;
        let marker_text = fs::read_to_string(&marker).unwrap();
        assert_eq!(
            marker_text.split("---\n\n").nth(1).unwrap().trim_end(),
            MARKER_BODY
        );
        assert!(!paths.root.join("hooks.json").exists());
        let canonical_skill_root = fs::canonicalize(&paths.generated_skills).unwrap();
        let refresh_observed = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let requests = server_for_run.received().await;
                let refreshed = requests
                    .iter()
                    .filter(|request| request["method"] == "skills/list")
                    .count();
                let task_cwd_refreshes = requests
                    .iter()
                    .filter(|request| {
                        request["method"] == "skills/list"
                            && request["params"]["cwds"]
                                .as_array()
                                .is_some_and(|cwds| {
                                    cwds.iter().any(|cwd| {
                                        cwd.as_str()
                                            == preexisting_idle_task_cwd_for_run.to_str()
                                    })
                                })
                    })
                    .count();
                let attached = requests.iter().any(|request| {
                    request["method"] == "skills/extraRoots/set"
                        && request["params"]["extraRoots"]
                            .as_array()
                            .is_some_and(|roots| {
                                roots.iter().any(|root| root.as_str() == canonical_skill_root.to_str())
                            })
                });
                if attached && refreshed >= 2 && task_cwd_refreshes >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            refresh_observed.is_ok(),
            "hot hook did not trigger a skill refresh; requests={:#?}",
            server_for_run.received().await
        );
        assert!(
            !server_for_run.received().await.iter().any(|request| {
                request["method"] == "thread/resume"
                    && request["params"]["threadId"] == "idle-thread"
            }),
            "Warden must not resume every unarchived idle task"
        );

        server_for_run
            .emit_notification(
                "turn/started",
                json!({"threadId":"thread","turn":{"id":"selected","status":"inProgress","items":[
                    {"id":"user-selected","type":"userMessage","content":[
                        {"type":"text","text":format!("[$record-events]({}) watch this turn", marker.display())}
                    ]}
                ]}}),
            )
            .await;
        server_for_run
            .emit_notification(
                "item/started",
                json!({"threadId":"thread","turnId":"selected","item":{"id":"tool","type":"commandExecution","status":"inProgress"}}),
            )
            .await;
        server_for_run
            .emit_notification(
                "item/completed",
                json!({"threadId":"thread","turnId":"selected","item":{"id":"tool","type":"commandExecution","status":"completed"}}),
            )
            .await;
        server_for_run
            .emit_notification(
                "item/completed",
                json!({"threadId":"thread","turnId":"selected","item":{"id":"message","type":"agentMessage","text":"done"}}),
            )
            .await;
        server_for_run
            .emit_notification(
                "turn/completed",
                json!({"threadId":"thread","turn":{"id":"selected","status":"completed"}}),
            )
            .await;
        wait_until(Duration::from_secs(10), || line_count(&output) == 6).await;
        wait_until(Duration::from_secs(3), || listed_threads_output.is_file()).await;
        let listed_threads: serde_json::Value =
            serde_json::from_slice(&fs::read(&listed_threads_output).unwrap()).unwrap();
        assert_eq!(
            listed_threads["idle-thread"]["cwd"],
            preexisting_idle_task_cwd_for_run.to_str().unwrap()
        );
        assert_eq!(
            listed_threads["idle-thread"]["status"]["type"],
            "idle"
        );

        server_for_run
            .emit_notification(
                "turn/started",
                json!({"threadId":"thread","turn":{"id":"plain","status":"inProgress","items":[
                    {"id":"user-plain","type":"userMessage","content":[{"type":"text","text":"no marker"}]}
                ]}}),
            )
            .await;
        server_for_run
            .emit_notification(
                "item/completed",
                json!({"threadId":"thread","turnId":"plain","item":{"id":"other","type":"commandExecution","status":"completed"}}),
            )
            .await;
        server_for_run
            .emit_notification(
                "turn/completed",
                json!({"threadId":"thread","turn":{"id":"plain","status":"completed"}}),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(line_count(&output), 6);

        let lines = fs::read_to_string(&output).unwrap();
        assert!(lines.contains("\"kind\":\"user_prompt_submitted\""));
        assert!(lines.contains("\"kind\":\"post_tool_use\""));
        assert!(lines.contains("\"kind\":\"agent_message_completed\""));
        assert!(lines.contains("\"raw_method\":\"item/completed\""));

        let health = daemon_health(&paths.action_socket).await;
        assert_eq!(
            health["daemon"]["hooks_ready"],
            2,
            "the hot-created hook and bundled template must both be ready"
        );
        assert!(health["daemon"]["last_processed_sequence"].as_u64().unwrap() > 0);
        assert_eq!(health["daemon"]["coverage_gap_count"], 0);

        let last_sequence = health["daemon"]["last_processed_sequence"]
            .as_u64()
            .unwrap();
        let refreshes_before_edit = server_for_run
            .received()
            .await
            .iter()
            .filter(|request| request["method"] == "skills/list")
            .count();
        let hook_file = hook_dir.join("hook.py");
        let mut source = fs::read_to_string(&hook_file).unwrap();
        source.push_str("\n# trigger a valid hot revision after lifecycle activity\n");
        fs::write(&hook_file, source).unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let refreshes = server_for_run
                    .received()
                    .await
                    .iter()
                    .filter(|request| request["method"] == "skills/list")
                    .count();
                if refreshes > refreshes_before_edit {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("hot hook edit did not trigger a skill refresh");
        let health_after_refresh = daemon_health(&paths.action_socket).await;
        assert!(
            health_after_refresh["daemon"]["last_processed_sequence"]
                .as_u64()
                .unwrap()
                >= last_sequence,
            "a registry refresh must not reset lifecycle health"
        );
        assert_eq!(health_after_refresh["daemon"]["coverage_gap_count"], 0);

        let _ = shutdown_tx.send(());
        runtime.await.unwrap().unwrap();
        assert!(!paths.action_socket.exists());
    })
    .await
    .unwrap();
    server.shutdown().await;
}
