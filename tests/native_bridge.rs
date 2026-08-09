#![cfg(unix)]

use serde_json::{Value, json};
use std::{fs, path::PathBuf, time::Duration};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};

fn bridge() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python/warden_native_bridge.py")
}

#[tokio::test]
async fn bridge_authenticates_and_forwards_one_bounded_native_event() {
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("warden.sock");
    let credential = temp.path().join("bridge-auth");
    fs::write(&credential, "bridge-secret\n").unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut line = String::new();
        BufReader::new(reader).read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "warden.native_hook.event");
        assert_eq!(request["bridge_auth"], "bridge-secret");
        assert_eq!(request["params"]["hook_event_name"], "PreToolUse");
        let response = json!({
            "type": "response",
            "protocol_version": 1,
            "id": request["id"],
            "ok": true,
            "result": {}
        });
        writer
            .write_all(format!("{}\n", response).as_bytes())
            .await
            .unwrap();
    });

    let mut child = tokio::process::Command::new("python3")
        .arg(bridge())
        .args(["--socket", socket.to_str().unwrap()])
        .args(["--credential-file", credential.to_str().unwrap()])
        .args(["--timeout", "2"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::to_string(&json!({
                "hook_event_name": "PreToolUse",
                "session_id": "thread",
                "turn_id": "turn",
                "tool_use_id": "tool"
            }))
            .unwrap()
            .as_bytes(),
        )
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn bridge_fails_open_for_invalid_or_oversized_input() {
    let temp = TempDir::new().unwrap();
    let credential = temp.path().join("bridge-auth");
    let missing_socket = temp.path().join("missing.sock");
    fs::write(&credential, "bridge-secret\n").unwrap();
    for input in [b"not-json".to_vec(), vec![b'x'; 1024 * 1024 + 1]] {
        let mut child = tokio::process::Command::new("python3")
            .arg(bridge())
            .args(["--socket", missing_socket.to_str().unwrap()])
            .args(["--credential-file", credential.to_str().unwrap()])
            .args(["--timeout", "0.1"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&input).await.unwrap();
        let output = child.wait_with_output().await.unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("warden bridge:"));
    }
}
