//! Opt-in live check against the installed Codex hook engine and Warden daemon.
//!
//! This starts one ephemeral real Codex inference. Run deliberately with:
//! `WARDEN_LIVE_TEST=1 cargo test --test live_native_blocking -- --ignored --nocapture`.

#[cfg(target_os = "macos")]
mod macos {
    use serde_json::Value;
    use std::{fs, path::PathBuf, process::Stdio, time::Duration};
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
    };

    struct HookCleanup(PathBuf);

    impl Drop for HookCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn source(output: &std::path::Path, delay: f64, version: u32) -> String {
        let output = serde_json::to_string(output.to_str().unwrap()).unwrap();
        format!(
            "import json, time\nfrom pathlib import Path\nfrom warden import hook, HookEventKind\nOUTPUT = Path({output})\n@hook(on=[HookEventKind.USER_PROMPT_SUBMITTED], blocking=True)\ndef run(event):\n    started = time.time()\n    time.sleep({delay})\n    with OUTPUT.open('a', encoding='utf-8') as stream:\n        stream.write(json.dumps({{'version': {version}, 'elapsed': time.time() - started, 'origin': event.origin}}) + '\\n')\n"
        )
    }

    async fn wait_for(path: &std::path::Path, exists: bool) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while path.exists() != exists {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_for_current_source(home: &std::path::Path, expected: &str) {
        let hook_revisions = home.join("runtimes/revisions/live-native-blocking-test");
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let source = fs::read(hook_revisions.join("current.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .and_then(|manifest| manifest["revision"].as_str().map(str::to_owned))
                    .and_then(|revision| {
                        fs::read_to_string(hook_revisions.join(revision).join("source/hook.py"))
                            .ok()
                    });
                if source.as_deref() == Some(expected) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "starts a real Codex inference and mutates only a named temporary Warden hook"]
    async fn installed_codex_waits_and_warden_hot_updates_without_restart() {
        assert_eq!(std::env::var("WARDEN_LIVE_TEST").as_deref(), Ok("1"));
        let home = std::env::var_os("WARDEN_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".warden")))
            .unwrap();
        let hook_dir = home.join("warden-hooks/live-native-blocking-test");
        let _cleanup = HookCleanup(hook_dir.clone());
        fs::create_dir_all(&hook_dir).unwrap();
        let temp = TempDir::new().unwrap();
        let output = temp.path().join("invocations.jsonl");
        let version_one = source(&output, 1.0, 1);
        fs::write(hook_dir.join("hook.py"), &version_one).unwrap();
        let marker = home.join("generated-skills/live-native-blocking-test/SKILL.md");
        let socket = home.join("warden.sock");
        wait_for(&marker, true).await;
        wait_for_current_source(&home, &version_one).await;

        let health = tokio::process::Command::new(env!("CARGO_BIN_EXE_warden"))
            .args(["--socket", socket.to_str().unwrap(), "health"])
            .output()
            .await
            .unwrap();
        let health: Value = serde_json::from_slice(&health.stdout).unwrap();
        assert_eq!(health["daemon"]["bridge"]["trusted"], true);

        let prompt = format!(
            "[$live-native-blocking-test]({}) Reply only OK.",
            marker.display()
        );
        let codex = tokio::process::Command::new("codex")
            .args(["exec", "--ephemeral", "--skip-git-repo-check", "-C", "/tmp"])
            .arg(prompt)
            .output()
            .await
            .unwrap();
        assert!(
            codex.status.success(),
            "{}",
            String::from_utf8_lossy(&codex.stderr)
        );
        let first: Value =
            serde_json::from_str(fs::read_to_string(&output).unwrap().lines().next().unwrap())
                .unwrap();
        assert_eq!(first["version"], 1);
        assert_eq!(first["origin"], "native");
        assert!(first["elapsed"].as_f64().unwrap() >= 0.9);

        let version_two = source(&output, 0.1, 2);
        fs::write(hook_dir.join("hook.py"), &version_two).unwrap();
        wait_for_current_source(&home, &version_two).await;
        let credential = home.join("bridge-auth");
        let proxy_path = temp.path().join("bridge-proxy.sock");
        let proxy_listener = UnixListener::bind(&proxy_path).unwrap();
        let upstream_socket = socket.clone();
        let proxy = tokio::spawn(async move {
            let (inbound, _) = proxy_listener.accept().await.unwrap();
            let mut inbound = BufReader::new(inbound);
            let mut request = String::new();
            inbound.read_line(&mut request).await.unwrap();
            let upstream = UnixStream::connect(upstream_socket).await.unwrap();
            let mut upstream = BufReader::new(upstream);
            upstream
                .get_mut()
                .write_all(request.as_bytes())
                .await
                .unwrap();
            let mut response = String::new();
            upstream.read_line(&mut response).await.unwrap();
            inbound
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
            serde_json::from_str::<Value>(&response).unwrap()
        });
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let session_id = format!("live-hot-update-{test_id}");
        let turn_id = format!("v2-{test_id}");
        let mut bridge = tokio::process::Command::new("python3")
            .arg(home.join("native-hooks/bridge.py"))
            .args(["--socket", proxy_path.to_str().unwrap()])
            .args(["--credential-file", credential.to_str().unwrap()])
            .args(["--timeout", "5"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        bridge.stdin.take().unwrap().write_all(
            serde_json::to_string(&serde_json::json!({
                "hook_event_name": "UserPromptSubmit", "session_id": session_id,
                "turn_id": turn_id, "cwd": "/tmp", "model": "test",
                "permission_mode": "default", "prompt": format!("[$live-native-blocking-test]({}) hot", marker.display())
            })).unwrap().as_bytes()
        ).await.unwrap();
        let bridge_output = bridge.wait_with_output().await.unwrap();
        assert!(bridge_output.status.success());
        let bridge_response = proxy.await.unwrap();
        assert_eq!(bridge_response["ok"], true);
        assert_eq!(bridge_response["result"]["blocking"], 1);
        let records = fs::read_to_string(&output).unwrap();
        let second: Value = serde_json::from_str(records.lines().last().unwrap()).unwrap();
        assert_eq!(second["version"], 2);
        assert!(second["elapsed"].as_f64().unwrap() < 0.5);

        drop(_cleanup);
        wait_for(&marker, false).await;
    }
}
