//! Opt-in compatibility check against the installed Codex Desktop app-server.
//!
//! This starts a real Codex turn and is therefore ignored unless deliberately selected with
//! `WARDEN_LIVE_TEST=1 WARDEN_LIVE_THREAD_ID=... cargo test --test live_macos -- --ignored`.

#[cfg(target_os = "macos")]
mod macos {
    use codex_control::{CodexControl, Config as ControlConfig};
    use serde_json::json;
    use std::{fs, path::Path, time::Duration};
    use tempfile::TempDir;
    use tokio::sync::oneshot;
    use warden_daemon::{Config, DataPaths, Warden};

    #[tokio::test]
    #[ignore = "starts a real Codex Desktop turn; see module documentation"]
    async fn installed_app_server_refreshes_and_explicit_marker_activates() {
        assert_eq!(
            std::env::var("WARDEN_LIVE_TEST").as_deref(),
            Ok("1"),
            "set WARDEN_LIVE_TEST=1 to acknowledge this live test"
        );
        let thread_id = std::env::var("WARDEN_LIVE_THREAD_ID")
            .expect("set WARDEN_LIVE_THREAD_ID to an existing disposable Codex task");
        let temp = TempDir::new().unwrap();
        let output = temp.path().join("live-events.jsonl");
        let paths = DataPaths::under(temp.path().join("warden"));
        paths.create_all().unwrap();
        let hook_dir = paths.hooks.join("live-warden-marker");
        fs::create_dir_all(&hook_dir).unwrap();
        let output_literal = serde_json::to_string(output.to_str().unwrap()).unwrap();
        fs::write(
            hook_dir.join("hook.py"),
            format!(
                "import json\nfrom warden import hook, HookEventKind\n@hook(on=[HookEventKind.USER_PROMPT_SUBMITTED, HookEventKind.TURN_STARTED])\ndef run(event):\n    with open({output_literal}, 'a', encoding='utf-8') as stream:\n        stream.write(json.dumps(event.to_dict()) + '\\n')\n"
            ),
        )
        .unwrap();
        let config = Config {
            paths: paths.clone(),
            codex_home: temp.path().join("codex-home"),
            agents_home: temp.path().join("agents-home"),
            python_sdk: Path::new(env!("CARGO_MANIFEST_DIR")).join("python"),
            ..Config::default()
        };

        CodexControl::run(ControlConfig::default(), move |handle| async move {
            let runtime_handle = handle.clone();
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            let runtime = tokio::spawn(Warden::serve_until(
                config,
                runtime_handle,
                async move {
                    let _ = shutdown_rx.await;
                },
            ));
            let marker = paths
                .generated_skills
                .join("live-warden-marker/SKILL.md");
            tokio::time::timeout(Duration::from_secs(30), async {
                while !marker.is_file() || !paths.action_socket.exists() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("Warden did not publish its marker against the installed app-server");

            let outcome = handle
                .start(
                    thread_id,
                    vec![
                        json!({"type":"skill","name":"live-warden-marker","path":marker}),
                        json!({"type":"text","text":"Warden live compatibility test. Reply only: ok"}),
                    ],
                )
                .await;
            eprintln!("live start outcome: {outcome:?}");
            tokio::time::timeout(Duration::from_secs(60), async {
                while fs::read_to_string(&output)
                    .map(|contents| contents.lines().count())
                    .unwrap_or(0)
                    < 2
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("installed app-server did not deliver explicit marker activation");
            let _ = shutdown_tx.send(());
            runtime.await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }
}
