use codex_control::{CodexControl, Config as ControlConfig};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tempfile::TempDir;
use tokio::sync::oneshot;
use transport::mock::MockAppServer;
use warden_daemon::{Config, DataPaths, Warden};

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

async fn wait_for(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn first_startup_attaches_markers_trusts_exact_bridge_hashes_and_reports_restart() {
    let temp = TempDir::new().unwrap();
    let rpc_socket = temp.path().join("rpc.sock");
    let server = MockAppServer::start(rpc_socket.clone()).await.unwrap();
    let paths = DataPaths::under(temp.path().join("warden"));
    let mut config = Config {
        paths: paths.clone(),
        codex_home: temp.path().join("codex"),
        manage_gui: false,
        ..Config::default()
    };
    config.python_sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");

    let hooks_file = config.codex_home.join("hooks.json");
    let command = format!(
        "python3 '{}' --socket '{}' --credential-file '{}' --timeout 605",
        paths.native_hooks.join("bridge.py").display(),
        paths.action_socket.display(),
        paths.bridge_credential.display(),
    );
    let hooks = warden_daemon::native_hook::BRIDGE_EVENTS
        .iter()
        .enumerate()
        .map(|(index, event)| {
            json!({
                "key": format!("warden-{index}"),
                "eventName": event,
                "command": command,
                "sourcePath": hooks_file,
                "enabled": true,
                "currentHash": format!("sha256:exact-{index}"),
                "trustStatus": "untrusted",
                "executionMode": "sync"
            })
        })
        .collect::<Vec<_>>();
    server
        .set_hooks_response(json!({"data": [{
            "cwd": paths.root,
            "hooks": hooks,
            "warnings": [],
            "errors": []
        }]}))
        .await;

    let server_for_run = server.clone();
    let socket_for_health = paths.action_socket.clone();
    CodexControl::run(control_config(&rpc_socket), move |handle| async move {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let runtime = tokio::spawn(Warden::serve_until(config, handle, async move {
            let _ = shutdown_rx.await;
        }));
        wait_for(&socket_for_health).await;
        assert!(
            paths
                .root
                .parent()
                .unwrap()
                .join("codex/skills/create-warden-hook/SKILL.md")
                .is_file()
        );
        assert!(paths.native_hooks.join("bridge.py").is_file());

        let health = tokio::process::Command::new(env!("CARGO_BIN_EXE_warden"))
            .args(["--socket", socket_for_health.to_str().unwrap(), "health"])
            .output()
            .await
            .unwrap();
        assert!(health.status.success());
        let health: Value = serde_json::from_slice(&health.stdout).unwrap();
        assert_eq!(health["daemon"]["bridge"]["configured"], true);
        assert_eq!(health["daemon"]["bridge"]["trusted"], true);
        assert_eq!(health["daemon"]["bridge"]["loaded_confirmed"], false);
        assert_eq!(health["daemon"]["bridge"]["restart_required"], true);

        let requests = server_for_run.received().await;
        let trust = requests
            .iter()
            .find(|request| request["method"] == "config/batchWrite")
            .expect("startup did not trust the bridge");
        for index in 0..4 {
            assert_eq!(
                trust["params"]["edits"][0]["value"][format!("warden-{index}")]["trusted_hash"],
                format!("sha256:exact-{index}")
            );
        }
        let generated_skills = std::fs::canonicalize(&paths.generated_skills).unwrap();
        assert!(requests.iter().any(|request| {
            request["method"] == "skills/extraRoots/set"
                && request["params"]["extraRoots"]
                    .as_array()
                    .is_some_and(|roots| roots.iter().any(|root| root == &json!(generated_skills)))
        }));

        let _ = shutdown_tx.send(());
        runtime.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    server.shutdown().await;
}
