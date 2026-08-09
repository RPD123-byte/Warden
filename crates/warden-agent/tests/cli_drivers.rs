mod common;

use std::{fs, sync::Arc, time::Duration};

use serde_json::json;
use tempfile::tempdir;
use tokio::time::sleep;
use uuid::Uuid;
use warden_agent::{
    AgentError, AgentInput, AgentRequest, ClaudeCliDriver, CodexCliDriver, Conversation,
    InvocationEnvironment, ProviderDriver, ProviderKind, ResumeMetadata,
};

use common::{shell_config, write_script};

const CLAUDE_SCRIPT: &str = r#"
input=$(cat)
printf '%s\n' "$*" >> "$ARGS_LOG"
printf '%s\n' "$input" >> "$INPUT_LOG"
printf '%s\n' '{"type":"system","session_id":"claude-session"}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"risk\":\"low\"}","session_id":"claude-session","usage":{"input_tokens":10}}'
"#;

const CODEX_SCRIPT: &str = r#"
input=$(cat)
printf '%s\n' "$*" >> "$ARGS_LOG"
printf '%s\n' "$input" >> "$INPUT_LOG"
printf '%s\n' '{"type":"thread.started","thread_id":"codex-thread"}'
printf '%s\n' '{"type":"turn.started"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"risk\":\"medium\"}"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":12}}'
"#;

#[cfg(unix)]
const DESCENDANT_SCRIPT: &str = r#"
trap '' TERM
(
  trap '' TERM
  while :; do sleep 1; done
) &
echo "$!" > "$DESCENDANT_PID"
cat >/dev/null
wait
"#;

#[cfg(unix)]
async fn wait_for_descendant_pid(path: &std::path::Path) -> i32 {
    for _ in 0..100 {
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<i32>()
            && pid > 0
        {
            return pid;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("fake provider never recorded its descendant pid");
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

#[cfg(unix)]
async fn assert_process_exited(pid: i32) {
    // A killed grandchild can remain visible very briefly while its new parent reaps it.
    for _ in 0..100 {
        if !process_exists(pid) {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("provider descendant {pid} survived process-group cleanup");
}

#[tokio::test]
async fn claude_uses_structured_output_and_does_not_persist_fresh_calls() {
    let directory = tempdir().expect("temp dir");
    let script = write_script(&directory, "claude.sh", CLAUDE_SCRIPT);
    let args_log = directory.path().join("args.log");
    let input_log = directory.path().join("input.log");
    let config = shell_config(&script, Duration::from_secs(2))
        .with_env("ARGS_LOG", &args_log)
        .with_env("INPUT_LOG", &input_log);
    let driver = ClaudeCliDriver::new(config);
    let input = AgentInput::new(
        41,
        json!({"kind":"post_tool_use","payload":{"result":"ok"}}),
    )
    .with_prompt("Find hidden risks.");

    let response = driver
        .invoke(AgentRequest::fresh(input.clone()))
        .await
        .expect("fake Claude succeeds");

    let args = fs::read_to_string(args_log).expect("args log");
    assert!(args.contains("--print"));
    assert!(args.contains("--output-format stream-json"));
    assert!(args.contains("--no-session-persistence"));
    assert!(!args.contains("--bare"));
    assert_eq!(
        fs::read_to_string(input_log).expect("input log").trim(),
        input.user_message().expect("message")
    );
    assert_eq!(response.provider, ProviderKind::Claude);
    assert_eq!(response.source_sequence, 41);
    assert_eq!(response.text.as_deref(), Some("{\"risk\":\"low\"}"));
    assert_eq!(response.structured_output, Some(json!({"risk":"low"})));
    assert_eq!(response.usage, Some(json!({"input_tokens":10})));
    assert!(response.resume.is_none());
    assert_eq!(response.events.len(), 2);
}

#[tokio::test]
async fn claude_starts_then_resumes_a_persistent_session() {
    let directory = tempdir().expect("temp dir");
    let script = write_script(&directory, "claude.sh", CLAUDE_SCRIPT);
    let args_log = directory.path().join("args.log");
    let input_log = directory.path().join("input.log");
    let config = shell_config(&script, Duration::from_secs(2))
        .with_env("ARGS_LOG", &args_log)
        .with_env("INPUT_LOG", &input_log);
    let driver = ClaudeCliDriver::new(config);

    let first = driver
        .invoke(AgentRequest::persistent(
            AgentInput::new(1, json!({"event":1})),
            None,
        ))
        .await
        .expect("start session");
    let resume = first.resume.expect("Claude resume metadata");
    assert_eq!(resume.session_id, "claude-session");

    driver
        .invoke(AgentRequest::persistent(
            AgentInput::new(2, json!({"event":2})),
            Some(resume),
        ))
        .await
        .expect("resume session");

    let invocations: Vec<_> = fs::read_to_string(args_log)
        .expect("args log")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(invocations.len(), 2);
    assert!(invocations[0].contains("--session-id"));
    assert!(!invocations[0].contains("--no-session-persistence"));
    assert!(invocations[1].contains("--resume claude-session"));
}

#[tokio::test]
async fn codex_uses_jsonl_and_resume_subcommand() {
    let directory = tempdir().expect("temp dir");
    let script = write_script(&directory, "codex.sh", CODEX_SCRIPT);
    let args_log = directory.path().join("args.log");
    let input_log = directory.path().join("input.log");
    let config = shell_config(&script, Duration::from_secs(2))
        .with_env("ARGS_LOG", &args_log)
        .with_env("INPUT_LOG", &input_log);
    let driver = CodexCliDriver::new(config);

    let fresh = driver
        .invoke(AgentRequest::fresh(AgentInput::new(
            10,
            json!({"kind":"agent_message_completed"}),
        )))
        .await
        .expect("fresh Codex call");
    assert!(fresh.resume.is_none());
    assert_eq!(fresh.structured_output, Some(json!({"risk":"medium"})));

    let started = driver
        .invoke(AgentRequest::persistent(
            AgentInput::new(11, json!({"event":11})),
            None,
        ))
        .await
        .expect("start Codex session");
    let resume = started.resume.expect("Codex resume metadata");
    assert_eq!(resume.session_id, "codex-thread");

    driver
        .invoke(AgentRequest::persistent(
            AgentInput::new(12, json!({"event":12})),
            Some(resume),
        ))
        .await
        .expect("resume Codex session");

    let invocations: Vec<_> = fs::read_to_string(args_log)
        .expect("args log")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(invocations.len(), 3);
    assert!(invocations[0].contains("exec --json --ephemeral"));
    assert!(!invocations[1].contains("--ephemeral"));
    assert!(invocations[2].contains("exec resume --json"));
    assert!(invocations[2].contains("codex-thread -"));
}

#[tokio::test]
async fn timeout_and_interrupt_terminate_only_the_target_invocation() {
    let directory = tempdir().expect("temp dir");
    let script = write_script(
        &directory,
        "slow.sh",
        "cat >/dev/null\nsleep 5\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\"}'\n",
    );
    let timed_driver = ClaudeCliDriver::new(shell_config(&script, Duration::from_millis(40)));
    let error = timed_driver
        .invoke(AgentRequest::fresh(AgentInput::new(
            1,
            json!({"slow":true}),
        )))
        .await
        .expect_err("call must time out");
    assert!(matches!(error, AgentError::Timeout { .. }));

    let driver = Arc::new(ClaudeCliDriver::new(shell_config(
        &script,
        Duration::from_secs(5),
    )));
    let invocation_id = Uuid::new_v4();
    let request = AgentRequest {
        invocation_id,
        input: AgentInput::new(2, json!({"slow":true})),
        conversation: Conversation::Fresh,
        environment: InvocationEnvironment::default(),
    };
    let running_driver = Arc::clone(&driver);
    let running = tokio::spawn(async move { running_driver.invoke(request).await });

    let mut interrupted = false;
    for _ in 0..20 {
        if driver.interrupt(invocation_id).await.expect("interrupt") {
            interrupted = true;
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(interrupted, "invocation should become visible to interrupt");
    let error = running
        .await
        .expect("join invocation")
        .expect_err("call must be interrupted");
    assert!(matches!(error, AgentError::Interrupted { .. }));
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_interrupt_shutdown_and_drop_do_not_leave_provider_descendants() {
    let directory = tempdir().expect("temp dir");
    let script = write_script(&directory, "descendant.sh", DESCENDANT_SCRIPT);

    let timeout_pid_path = directory.path().join("timeout.pid");
    let timeout_driver = ClaudeCliDriver::new(
        shell_config(&script, Duration::from_millis(50))
            .with_env("DESCENDANT_PID", &timeout_pid_path),
    );
    let timeout_error = timeout_driver
        .invoke(AgentRequest::fresh(AgentInput::new(1, json!({}))))
        .await
        .expect_err("provider tree must time out");
    assert!(matches!(timeout_error, AgentError::Timeout { .. }));
    assert_process_exited(wait_for_descendant_pid(&timeout_pid_path).await).await;

    let interrupt_pid_path = directory.path().join("interrupt.pid");
    let interrupt_driver = Arc::new(ClaudeCliDriver::new(
        shell_config(&script, Duration::from_secs(5))
            .with_env("DESCENDANT_PID", &interrupt_pid_path),
    ));
    let interrupt_id = Uuid::new_v4();
    let interrupt_request = AgentRequest {
        invocation_id: interrupt_id,
        input: AgentInput::new(2, json!({})),
        conversation: Conversation::Fresh,
        environment: InvocationEnvironment::default(),
    };
    let running_driver = Arc::clone(&interrupt_driver);
    let interrupted = tokio::spawn(async move { running_driver.invoke(interrupt_request).await });
    let interrupt_descendant = wait_for_descendant_pid(&interrupt_pid_path).await;
    assert!(
        interrupt_driver
            .interrupt(interrupt_id)
            .await
            .expect("interrupt request")
    );
    assert!(matches!(
        interrupted.await.unwrap().unwrap_err(),
        AgentError::Interrupted { .. }
    ));
    assert_process_exited(interrupt_descendant).await;

    let shutdown_pid_path = directory.path().join("shutdown.pid");
    let shutdown_driver = Arc::new(ClaudeCliDriver::new(
        shell_config(&script, Duration::from_secs(5))
            .with_env("DESCENDANT_PID", &shutdown_pid_path),
    ));
    let shutdown_driver_for_call = Arc::clone(&shutdown_driver);
    let running = tokio::spawn(async move {
        shutdown_driver_for_call
            .invoke(AgentRequest::fresh(AgentInput::new(3, json!({}))))
            .await
    });
    let shutdown_descendant = wait_for_descendant_pid(&shutdown_pid_path).await;
    shutdown_driver
        .shutdown()
        .await
        .expect("shutdown waits for process-tree cleanup");
    assert!(matches!(
        running.await.unwrap().unwrap_err(),
        AgentError::Interrupted { .. }
    ));
    assert_process_exited(shutdown_descendant).await;

    let dropped_pid_path = directory.path().join("dropped.pid");
    let dropped_driver = Arc::new(ClaudeCliDriver::new(
        shell_config(&script, Duration::from_secs(5)).with_env("DESCENDANT_PID", &dropped_pid_path),
    ));
    let dropped_driver_for_call = Arc::clone(&dropped_driver);
    let dropped = tokio::spawn(async move {
        dropped_driver_for_call
            .invoke(AgentRequest::fresh(AgentInput::new(4, json!({}))))
            .await
    });
    let dropped_descendant = wait_for_descendant_pid(&dropped_pid_path).await;
    dropped.abort();
    assert!(
        dropped.await.unwrap_err().is_cancelled(),
        "provider invocation task should be cancelled"
    );
    assert_process_exited(dropped_descendant).await;

    // The cancellation guard must also unregister the invocation, so shutdown is immediate and
    // does not rely on its deadline to mask a leaked runner entry.
    dropped_driver
        .shutdown()
        .await
        .expect("shutdown after dropped invocation remains clean");
}

#[tokio::test]
async fn malformed_output_and_provider_mismatch_are_explicit() {
    let directory = tempdir().expect("temp dir");
    let script = write_script(
        &directory,
        "bad.sh",
        "cat >/dev/null\nprintf 'not-json\\n'\n",
    );
    let driver = ClaudeCliDriver::new(shell_config(&script, Duration::from_secs(1)));
    let malformed = driver
        .invoke(AgentRequest::fresh(AgentInput::new(1, json!({}))))
        .await
        .expect_err("malformed JSONL fails");
    assert!(matches!(malformed, AgentError::InvalidJsonLine { .. }));

    let mismatch = driver
        .invoke(AgentRequest::persistent(
            AgentInput::new(2, json!({})),
            Some(ResumeMetadata::new(ProviderKind::Codex, "wrong")),
        ))
        .await
        .expect_err("provider mismatch fails before spawning");
    assert!(matches!(mismatch, AgentError::ProviderMismatch { .. }));
}

#[tokio::test]
async fn invocation_environment_reaches_only_the_child_and_is_redacted() {
    let directory = tempdir().expect("temp dir");
    let script = write_script(
        &directory,
        "environment.sh",
        r#"
cat >/dev/null
printf '%s|%s|%s' "$WARDEN_SOCKET" "$WARDEN_INVOCATION_ID" "$WARDEN_TOKEN" > "$ENV_CAPTURE"
printf '{"type":"result","subtype":"success","is_error":false,"result":"%s","session_id":"unused"}\n' "$WARDEN_TOKEN"
"#,
    );
    let capture = directory.path().join("environment.log");
    let config = shell_config(&script, Duration::from_secs(1))
        .with_env("ENV_CAPTURE", &capture)
        .with_env("WARDEN_TOKEN", "static-value-must-be-overridden");
    let driver = ClaudeCliDriver::new(config);
    let environment = InvocationEnvironment::new()
        .with_var("WARDEN_SOCKET", "/tmp/warden-test.sock")
        .with_var("WARDEN_INVOCATION_ID", "invocation-42")
        .with_var("WARDEN_TOKEN", "super-secret-token");
    let request = AgentRequest::fresh(AgentInput::new(1, json!({"event":"authorized"})))
        .with_environment(environment);

    let debug = format!("{request:?}");
    assert!(debug.contains("WARDEN_TOKEN"));
    assert!(!debug.contains("super-secret-token"));
    let response = driver.invoke(request).await.expect("provider succeeds");

    assert_eq!(
        fs::read_to_string(capture).expect("environment capture"),
        "/tmp/warden-test.sock|invocation-42|super-secret-token"
    );
    let serialized = serde_json::to_string(&response).expect("serialize response");
    assert!(!serialized.contains("super-secret-token"));
    assert!(!serialized.contains("WARDEN_TOKEN"));
    assert_eq!(response.text.as_deref(), Some("<redacted>"));
}
