//! Opt-in end-to-end test of the installed Warden daemon's `pop-interrupt` hook.
//!
//! This creates and archives a disposable Codex task. Run deliberately with:
//! `WARDEN_LIVE_POP_TEST=1 cargo test --test live_pop_interrupt -- --ignored --nocapture`.

#[cfg(target_os = "macos")]
mod macos {
    use codex_control::{CodexControl, Config as ControlConfig};
    use serde_json::{Value, json};
    use std::{fs, path::PathBuf, time::Duration};
    use tempfile::TempDir;
    use tokio::sync::oneshot;
    use transport::{RpcClient, TransportConfig, TransportHandle};
    use warden_daemon::{Config as WardenConfig, DataPaths, Warden};

    const HOOK_ID: &str = "pop-interrupt";

    #[tokio::test]
    #[ignore = "creates a disposable real Codex task and invokes the installed Claude CLI"]
    async fn selected_marker_causes_claude_to_interrupt_the_turn() {
        codex_control::init_tracing("warden_daemon=debug,warn");
        assert_eq!(
            std::env::var("WARDEN_LIVE_POP_TEST").as_deref(),
            Ok("1"),
            "set WARDEN_LIVE_POP_TEST=1 to acknowledge this live test"
        );
        let home = dirs::home_dir().expect("home directory");
        let installed_root = home.join(".warden");
        let installed_hook = installed_root.join("warden-hooks/pop-interrupt/hook.py");
        assert!(
            installed_hook.is_file(),
            "missing hook: {}",
            installed_hook.display()
        );
        let temp = TempDir::new().expect("temporary Warden home");
        let paths = DataPaths::under(temp.path().join("warden"));
        let hook_dir = paths.hooks.join(HOOK_ID);
        fs::create_dir_all(&hook_dir).unwrap();
        fs::copy(&installed_hook, hook_dir.join("hook.py")).unwrap();
        let warden_config = WardenConfig {
            paths: paths.clone(),
            codex_home: temp.path().join("codex-home"),
            agents_home: temp.path().join("agents-home"),
            python_sdk: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python"),
            ..WardenConfig::default()
        };
        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let installed_skill_root = fs::canonicalize(installed_root.join("generated-skills"))
            .expect("installed generated-skills root");
        // Create the task before Warden (and its CodexControl observer) starts. This
        // reproduces the production case where Warden attaches to an idle existing task.
        let boot = TransportHandle::spawn(TransportConfig::default());
        let created = boot
            .request("thread/start", json!({"cwd": cwd}))
            .await
            .expect("create disposable Codex task");
        let thread_id = created["thread"]["id"]
            .as_str()
            .expect("thread/start response has an id")
            .to_owned();

        CodexControl::run(
            ControlConfig {
                manage_gui: false,
                ..ControlConfig::default()
            },
            move |handle| async move {
                let (shutdown_tx, shutdown_rx) = oneshot::channel();
                let mut runtime = tokio::spawn(Warden::serve_until(
                    warden_config,
                    handle.clone(),
                    async move {
                        let _ = shutdown_rx.await;
                    },
                ));
                let marker = paths.generated_skills.join(HOOK_ID).join("SKILL.md");
                tokio::time::timeout(Duration::from_secs(30), async {
                    while !marker.is_file() || !paths.action_socket.exists() {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                })
                .await
                .expect("test Warden did not publish its marker and action gateway");
                let outcome: Result<(), String> = async {
                    let marked_prompt = format!(
                        "[$pop-interrupt]({}) POP. Reply immediately with `finished`.",
                        marker.display()
                    );
                    let started = handle
                        .start(
                            &thread_id,
                            vec![
                                json!({"type":"skill","name":HOOK_ID,"path":marker.clone()}),
                                json!({"type":"text","text":marked_prompt}),
                            ],
                        )
                        .await;
                    if matches!(started, codex_control::ActionOutcome::Rejected { .. }) {
                        return Err(format!("Codex rejected the disposable turn: {started:?}"));
                    }

                    let turn_id = tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            if let Some(turn_id) = handle
                                .snapshot()
                                .await
                                .threads
                                .get(&thread_id)
                                .and_then(|thread| thread.active_turn_id.clone())
                            {
                                break turn_id;
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    })
                    .await
                    .map_err(|_| "disposable turn never became active".to_owned())?;

                    let terminal = tokio::time::timeout(Duration::from_secs(70), async {
                        loop {
                            let events = handle.query_sequence(Some(&thread_id), 0, None).await;
                            if let Some(event) = events.events.iter().rev().find(|event| {
                                event.method() == Some("turn/completed")
                                    && event.turn_id.as_deref() == Some(&turn_id)
                            }) {
                                break event.frame.params().cloned().unwrap_or(Value::Null);
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    })
                    .await
                    .map_err(|_| {
                        "Warden did not terminate the disposable turn within 70 seconds".to_owned()
                    })?;
                    let status = terminal
                        .get("turn")
                        .and_then(|turn| turn.get("status"))
                        .and_then(Value::as_str)
                        .or_else(|| terminal.get("status").and_then(Value::as_str));
                    if !matches!(status, Some("interrupted" | "cancelled" | "canceled")) {
                        return Err(format!(
                            "turn ended without an interrupt; terminal payload: {terminal}"
                        ));
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
                    .request("thread/archive", json!({"threadId": thread_id}))
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
