//! Opt-in live smoke test for the bundled persistent Claude decision monitor.
//!
//! This creates and archives one disposable Codex task and invokes Claude Sonnet twice. Run with:
//! `WARDEN_LIVE_DECISION_TEST=1 cargo test --test live_decision_monitor -- --ignored --nocapture`.

#[cfg(target_os = "macos")]
mod macos {
    use codex_control::{CodexControl, Config as ControlConfig};
    use serde_json::{Value, json};
    use std::{fs, path::PathBuf, time::Duration};
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixStream,
        sync::oneshot,
    };
    use transport::{RpcClient, TransportConfig, TransportHandle};
    use uuid::Uuid;
    use warden_daemon::{
        Config as WardenConfig, DataPaths, Warden,
        action::{ACTION_PROTOCOL_VERSION, GatewayRequest, GatewayResponse},
    };

    const HOOK_ID: &str = "unspecified-decisions";

    async fn wait_for(path: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_for_active_turn(
        handle: &codex_control::Handle,
        thread_id: &str,
    ) -> Result<String, String> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(turn_id) = handle
                    .snapshot()
                    .await
                    .threads
                    .get(thread_id)
                    .and_then(|thread| thread.active_turn_id.clone())
                {
                    return turn_id;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| "disposable turn did not become active".to_owned())
    }

    async fn wait_for_interrupted(
        handle: &codex_control::Handle,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(), String> {
        let terminal = tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                let events = handle.query_sequence(Some(thread_id), 0, None).await;
                if let Some(event) = events.events.iter().rev().find(|event| {
                    event.method() == Some("turn/completed")
                        && event.turn_id.as_deref() == Some(turn_id)
                }) {
                    return event.frame.params().cloned().unwrap_or(Value::Null);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| "decision monitor did not terminate the disposable turn".to_owned())?;
        let status = terminal
            .get("turn")
            .and_then(|turn| turn.get("status"))
            .and_then(Value::as_str)
            .or_else(|| terminal.get("status").and_then(Value::as_str));
        if !matches!(status, Some("interrupted" | "cancelled" | "canceled")) {
            return Err(format!("turn ended without interruption: {terminal}"));
        }
        Ok(())
    }

    async fn wait_for_delivered_question(
        handle: &codex_control::Handle,
        thread_id: &str,
        interrupted_turn_id: &str,
        expected_question_text: &str,
    ) -> Result<String, String> {
        tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                let events = handle.query_sequence(Some(thread_id), 0, None).await;
                for delivery_turn in events.events.iter().filter_map(|event| {
                    (event.method() == Some("turn/started")
                        && event.turn_id.as_deref() != Some(interrupted_turn_id))
                    .then(|| event.turn_id.clone())
                    .flatten()
                }) {
                    let visible_question = events.events.iter().any(|event| {
                        event.turn_id.as_deref() == Some(&delivery_turn)
                            && event.frame.raw().to_string().contains("Warden supervisor stopped")
                            && event
                                .frame
                                .raw()
                                .to_string()
                                .contains(expected_question_text)
                    });
                    let completed = events.events.iter().any(|event| {
                        event.method() == Some("turn/completed")
                            && event.turn_id.as_deref() == Some(&delivery_turn)
                    });
                    if visible_question && completed {
                        return delivery_turn;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| {
            format!(
                "no completed follow-up turn visibly delivered question text {expected_question_text:?}"
            )
        })
    }

    async fn send_native(
        socket: &std::path::Path,
        credential: &str,
        params: Value,
    ) -> GatewayResponse {
        let request = GatewayRequest {
            message_type: "request".into(),
            protocol_version: ACTION_PROTOCOL_VERSION,
            id: Uuid::new_v4().simple().to_string(),
            method: "warden.native_hook.event".into(),
            params,
            context: None,
            bridge_auth: Some(credential.into()),
        };
        let mut encoded = serde_json::to_vec(&request).unwrap();
        encoded.push(b'\n');
        let mut stream = BufReader::new(UnixStream::connect(socket).await.unwrap());
        stream.get_mut().write_all(&encoded).await.unwrap();
        let mut response = String::new();
        stream.read_line(&mut response).await.unwrap();
        serde_json::from_str(&response).unwrap()
    }

    async fn activate_turn(
        socket: &std::path::Path,
        credential: &str,
        marker: &std::path::Path,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(), String> {
        let marker_name = marker
            .parent()
            .and_then(std::path::Path::file_name)
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("invalid marker path {}", marker.display()))?;
        let response = send_native(
            socket,
            credential,
            json!({
                "hook_event_name":"UserPromptSubmit",
                "session_id":thread_id,
                "turn_id":turn_id,
                "cwd":"/tmp",
                "prompt":format!("[${marker_name}]({}) monitor", marker.display()),
            }),
        )
        .await;
        if !response.ok
            || !response.result.as_ref().unwrap()["activated"]
                .as_array()
                .is_some_and(|hooks| hooks.iter().any(|hook| hook == HOOK_ID))
        {
            return Err(format!("could not activate bundled monitor: {response:?}"));
        }
        Ok(())
    }

    async fn send_native_post_tool(
        socket: &std::path::Path,
        credential: &str,
        thread_id: &str,
        turn_id: &str,
        claim: &str,
    ) -> GatewayResponse {
        send_native(
            socket,
            credential,
            json!({
                "hook_event_name":"PostToolUse",
                "session_id":thread_id,
                "turn_id":turn_id,
                "cwd":"/tmp",
                "tool_use_id":Uuid::new_v4().simple().to_string(),
                "tool_name":"implementation",
                "tool_input":{"operation":"apply implementation decision"},
                "tool_response":{"summary":claim},
            }),
        )
        .await
    }

    fn session_snapshot(paths: &DataPaths) -> Value {
        let provider = paths.sessions.join("claude");
        let records = fs::read_dir(provider)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "json")
            })
            .filter(|entry| !entry.path().to_string_lossy().ends_with("pending.json"))
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1, "expected one task-scoped Claude session");
        serde_json::from_slice(&fs::read(records[0].path()).unwrap()).unwrap()
    }

    #[tokio::test]
    #[ignore = "creates a disposable Codex task and verifies live start/pause/resume with Claude Sonnet"]
    async fn bundled_monitor_interrupts_delivers_questions_and_resumes_claude() {
        assert_eq!(
            std::env::var("WARDEN_LIVE_DECISION_TEST").as_deref(),
            Ok("1"),
            "set WARDEN_LIVE_DECISION_TEST=1 to acknowledge this live test"
        );
        let installed_root = dirs::home_dir().unwrap().join(".warden");
        let installed_skill_root = fs::canonicalize(installed_root.join("generated-skills"))
            .expect("installed generated-skills root");
        let temp = TempDir::new().unwrap();
        let paths = DataPaths::under(temp.path().join("warden"));
        let config = WardenConfig {
            paths: paths.clone(),
            codex_home: temp.path().join("codex-home"),
            python_sdk: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python"),
            ..WardenConfig::default()
        };
        let cwd = temp.path().join("implementation");
        fs::create_dir_all(&cwd).unwrap();

        let boot = TransportHandle::spawn(TransportConfig::default());
        let created = boot
            .request("thread/start", json!({"cwd":cwd}))
            .await
            .expect("create disposable Codex task");
        let thread_id = created["thread"]["id"].as_str().unwrap().to_owned();

        CodexControl::run(
            ControlConfig {
                manage_gui: false,
                ..ControlConfig::default()
            },
            move |handle| async move {
                let (shutdown_tx, shutdown_rx) = oneshot::channel();
                let mut runtime = tokio::spawn(Warden::serve_until(
                    config,
                    handle.clone(),
                    async move {
                        let _ = shutdown_rx.await;
                    },
                ));
                let start_marker = paths
                    .generated_skills
                    .join(format!("{HOOK_ID}-start"))
                    .join("SKILL.md");
                let pause_marker = paths
                    .generated_skills
                    .join(format!("{HOOK_ID}-pause"))
                    .join("SKILL.md");
                let resume_marker = paths
                    .generated_skills
                    .join(format!("{HOOK_ID}-resume"))
                    .join("SKILL.md");
                let stop_marker = paths
                    .generated_skills
                    .join(format!("{HOOK_ID}-stop"))
                    .join("SKILL.md");
                wait_for(&start_marker).await;
                wait_for(&pause_marker).await;
                wait_for(&resume_marker).await;
                wait_for(&stop_marker).await;
                wait_for(&paths.action_socket).await;
                let credential = fs::read_to_string(&paths.bridge_credential).unwrap();

                let outcome: Result<(), String> = async {
                    let first = handle
                        .start(
                            &thread_id,
                            vec![json!({"type":"text","text":"Implement some MNIST training ML pipeline in a completely different folder real quick. No other specification is provided. Keep this test turn active by running a shell sleep for 120 seconds."})],
                        )
                        .await;
                    if matches!(first, codex_control::ActionOutcome::Rejected { .. }) {
                        return Err(format!("Codex rejected first disposable turn: {first:?}"));
                    }
                    let first_turn = wait_for_active_turn(&handle, &thread_id).await?;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    activate_turn(
                        &paths.action_socket,
                        credential.trim(),
                        &start_marker,
                        &thread_id,
                        &first_turn,
                    )
                    .await?;
                    let started = std::time::Instant::now();
                    let first_review = send_native_post_tool(
                        &paths.action_socket,
                        credential.trim(),
                        &thread_id,
                        &first_turn,
                        "Codex selected PyTorch, a CNN architecture, a src/mnist_pipeline package layout, deterministic train/validation/test splitting, best-checkpoint persistence, and JSON metrics. Codex described these choices in commentary before creating the files, but the user did not select any of them.",
                    )
                    .await;
                    if !first_review.ok || first_review.result.as_ref().unwrap()["blocking"] != 1 {
                        return Err(format!("first blocking review failed: {first_review:?}"));
                    }
                    if started.elapsed() < Duration::from_millis(500) {
                        return Err("first Claude review did not hold the native barrier".into());
                    }
                    wait_for_interrupted(&handle, &thread_id, &first_turn).await?;
                    wait_for_delivered_question(
                        &handle,
                        &thread_id,
                        &first_turn,
                        "PyTorch",
                    )
                    .await?;
                    let first_snapshot = session_snapshot(&paths);
                    if first_snapshot["snapshot"]["model"] != "sonnet" {
                        return Err(format!("session was not bound to Sonnet: {first_snapshot}"));
                    }
                    let session_id = first_snapshot["snapshot"]["resume"]["session_id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let first_sequence = first_snapshot["snapshot"]
                        ["last_successful_source_sequence"]
                        .as_u64()
                        .unwrap();

                    let second = handle
                        .start(
                            &thread_id,
                            vec![json!({"type":"text","text":"Continue the monitoring test. The logging vendor and observability file layout are still unresolved and require my answer. Keep the turn active with another 120-second shell sleep."})],
                        )
                        .await;
                    if matches!(second, codex_control::ActionOutcome::Rejected { .. }) {
                        return Err(format!("Codex rejected second disposable turn: {second:?}"));
                    }
                    let second_turn = wait_for_active_turn(&handle, &thread_id).await?;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    activate_turn(
                        &paths.action_socket,
                        credential.trim(),
                        &pause_marker,
                        &thread_id,
                        &second_turn,
                    )
                    .await?;
                    let paused_review = send_native_post_tool(
                        &paths.action_socket,
                        credential.trim(),
                        &thread_id,
                        &second_turn,
                        "This event is emitted while the continuous supervisor is paused.",
                    )
                    .await;
                    if !paused_review.ok
                        || paused_review.result.as_ref().unwrap()["blocking"] != 0
                    {
                        return Err(format!("paused review unexpectedly ran: {paused_review:?}"));
                    }
                    activate_turn(
                        &paths.action_socket,
                        credential.trim(),
                        &resume_marker,
                        &thread_id,
                        &second_turn,
                    )
                    .await?;
                    let second_review = send_native_post_tool(
                        &paths.action_socket,
                        credential.trim(),
                        &thread_id,
                        &second_turn,
                        "Codex selected Datadog and created observability/datadog/client.py even though the logging vendor and file layout remain unapproved.",
                    )
                    .await;
                    if !second_review.ok || second_review.result.as_ref().unwrap()["blocking"] != 1 {
                        return Err(format!("second blocking review failed: {second_review:?}"));
                    }
                    wait_for_interrupted(&handle, &thread_id, &second_turn).await?;
                    wait_for_delivered_question(
                        &handle,
                        &thread_id,
                        &second_turn,
                        "Datadog",
                    )
                    .await?;
                    let second_snapshot = session_snapshot(&paths);
                    if second_snapshot["snapshot"]["resume"]["session_id"] != session_id {
                        return Err("second review did not resume the first Claude session".into());
                    }
                    if second_snapshot["snapshot"]["last_successful_source_sequence"]
                        .as_u64()
                        .unwrap()
                        <= first_sequence
                    {
                        return Err("persistent session cursor did not advance".into());
                    }
                    activate_turn(
                        &paths.action_socket,
                        credential.trim(),
                        &stop_marker,
                        &thread_id,
                        "continuous-stop-check",
                    )
                    .await?;
                    let stopped_review = send_native_post_tool(
                        &paths.action_socket,
                        credential.trim(),
                        &thread_id,
                        "continuous-stop-check",
                        "This event is emitted after continuous monitoring was stopped.",
                    )
                    .await;
                    if !stopped_review.ok
                        || stopped_review.result.as_ref().unwrap()["blocking"] != 0
                    {
                        return Err(format!("stopped review unexpectedly ran: {stopped_review:?}"));
                    }
                    Ok(())
                }
                .await;

                let _ = shutdown_tx.send(());
                match tokio::time::timeout(Duration::from_secs(10), &mut runtime).await {
                    Ok(joined) => joined.unwrap().unwrap(),
                    Err(_) => {
                        runtime.abort();
                        let _ = runtime.await;
                        panic!("test Warden did not shut down within 10 seconds");
                    }
                }
                handle
                    .set_skill_extra_roots([installed_skill_root])
                    .await
                    .expect("restore installed Warden skill root");
                let _ = boot
                    .request("thread/archive", json!({"threadId":thread_id}))
                    .await;
                boot.shutdown().await;
                if let Err(error) = outcome {
                    panic!("{error}");
                }
                Ok::<(), Box<dyn std::error::Error>>(())
            },
        )
        .await
        .unwrap()
        .unwrap();
    }
}
