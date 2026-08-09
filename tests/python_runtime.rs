use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use uuid::Uuid;
use warden_daemon::{
    Config, DataPaths, HookId, HookRegistry,
    action::InvocationCredential,
    event::{HookEventEnvelope, HookEventKind},
    python::{EnvironmentManager, PythonError, PythonRuntime},
};

fn config(temp: &TempDir) -> Config {
    Config {
        paths: DataPaths::under(temp.path().join("warden")),
        hook_timeout: Duration::from_secs(5),
        max_hook_message_bytes: 256 * 1024,
        ..Config::default()
    }
}

fn event(sequence: u64) -> HookEventEnvelope {
    HookEventEnvelope {
        sequence,
        origin: warden_daemon::event::HookEventOriginKind::Observed,
        source_sequence: Some(sequence),
        receipt_ordinal: sequence,
        native_event_name: None,
        kind: HookEventKind::UserPromptSubmitted,
        thread_id: Some("thread".into()),
        turn_id: Some("turn".into()),
        item_id: None,
        unix_receipt_ms: 1,
        emitted_at_ms: None,
        reconstructed: false,
        payload: json!({"message":"hello"}),
        raw_method: Some("turn/started".into()),
        raw_payload: json!({"jsonrpc":"2.0","method":"turn/started"}),
    }
}

fn credential(config: &Config) -> InvocationCredential {
    InvocationCredential {
        invocation_id: Uuid::new_v4(),
        token: "test-token".into(),
        socket: config.paths.action_socket.clone(),
    }
}

fn write_hook(root: &Path, name: &str, source: &str) {
    let directory = root.join(name);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("hook.py"), source).unwrap();
}

#[cfg(unix)]
fn write_executable(path: &Path, source: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
async fn assert_process_exits(pid_file: &Path) {
    let pid = fs::read_to_string(pid_file).unwrap();
    for _ in 0..50 {
        if !std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed-out child process {pid} is still running");
}

#[tokio::test]
async fn minimal_hook_invokes_and_hot_reload_keeps_old_revision_immutable() {
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    config.paths.create_all().unwrap();
    fs::write(
        config.paths.modules.join("shared_logic.py"),
        "VALUE = 'one'\n",
    )
    .unwrap();
    write_hook(
        &config.paths.hooks,
        "demo",
        "import os, shared_logic\nfrom warden import hook, HookEventKind\n@hook(on=HookEventKind.USER_PROMPT_SUBMITTED)\ndef run(event):\n    return {'version': 1, 'sequence': event.sequence, 'shared': shared_logic.VALUE, 'request_timeout': os.environ.get('WARDEN_REQUEST_TIMEOUT_SECONDS')}\n",
    );
    let python = Arc::new(PythonRuntime::new(&config));
    let registry = HookRegistry::new(
        config.paths.hooks.clone(),
        config.paths.modules.clone(),
        config.paths.generated_skills.clone(),
        config.paths.runtimes.clone(),
        python.clone(),
    );
    registry.refresh().await.unwrap();
    let id = HookId::parse("demo").unwrap();
    let first = registry.current(&id).await.unwrap();
    let result = python
        .invoke(first.clone(), event(1), credential(&config))
        .await
        .unwrap();
    assert_eq!(
        result.result,
        json!({"version":1,"sequence":1,"shared":"one","request_timeout":"5"})
    );

    write_hook(
        &config.paths.hooks,
        "demo",
        "import shared_logic\nfrom warden import hook, HookEventKind\nfrom warden.modules import claude\n@hook(on=HookEventKind.USER_PROMPT_SUBMITTED)\ndef run(event):\n    return {'version': 2, 'module': claude.__name__, 'shared': shared_logic.VALUE}\n",
    );
    registry.refresh().await.unwrap();
    let second = registry.current(&id).await.unwrap();
    assert_ne!(first.revision, second.revision);
    let old_result = python
        .invoke(first, event(2), credential(&config))
        .await
        .unwrap();
    let new_result = python
        .invoke(second.clone(), event(3), credential(&config))
        .await
        .unwrap();
    assert_eq!(old_result.result["version"], 1);
    assert_eq!(new_result.result["version"], 2);
    assert_eq!(new_result.result["module"], "warden.modules.claude");
    assert_eq!(new_result.result["shared"], "one");

    fs::write(
        config.paths.modules.join("shared_logic.py"),
        "VALUE = 'two'\n",
    )
    .unwrap();
    registry.refresh().await.unwrap();
    let third = registry.current(&id).await.unwrap();
    assert_ne!(second.revision, third.revision);
    let third_result = python
        .invoke(third, event(4), credential(&config))
        .await
        .unwrap();
    assert_eq!(third_result.result["shared"], "two");
    python.shutdown().await;
}

#[tokio::test]
async fn dependency_content_rebuilds_environment_and_failure_is_revision_local() {
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    config.paths.create_all().unwrap();
    let hook_dir = config.paths.hooks.join("deps");
    write_hook(
        &config.paths.hooks,
        "deps",
        "import setuptools\nfrom warden import hook, HookEventKind\n@hook(on=HookEventKind.USER_PROMPT_SUBMITTED)\ndef run(event): return setuptools.__version__\n",
    );
    fs::write(hook_dir.join("requirements.txt"), "setuptools\n").unwrap();
    let environments = EnvironmentManager::new(
        config.python.clone(),
        config.python_sdk.clone(),
        config.paths.runtimes.clone(),
    );
    let first = environments.prepare(&hook_dir).await.unwrap();
    fs::write(
        hook_dir.join("requirements.txt"),
        "setuptools\n# dependency set two\n",
    )
    .unwrap();
    let second = environments.prepare(&hook_dir).await.unwrap();
    assert_ne!(first.hash, second.hash);
    assert!(first.python.is_file() && second.python.is_file());

    let python = Arc::new(PythonRuntime::new(&config));
    let registry = HookRegistry::new(
        config.paths.hooks.clone(),
        config.paths.modules.clone(),
        config.paths.generated_skills.clone(),
        config.paths.runtimes.clone(),
        python.clone(),
    );
    registry.refresh().await.unwrap();
    let id = HookId::parse("deps").unwrap();
    let valid = registry.current(&id).await.unwrap();
    assert!(
        python
            .invoke(valid.clone(), event(1), credential(&config))
            .await
            .unwrap()
            .result
            .as_str()
            .is_some()
    );

    fs::write(
        hook_dir.join("requirements.txt"),
        "/definitely/missing/warden-package\n",
    )
    .unwrap();
    let delta = registry.refresh().await.unwrap();
    assert_eq!(delta.failed.len(), 1);
    assert_eq!(
        registry.current(&id).await.unwrap().revision,
        valid.revision
    );
    python.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn environment_commands_have_bounded_time_output_and_child_lifetime() {
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    let hook_dir = temp.path().join("hook");
    fs::create_dir_all(&hook_dir).unwrap();
    let pid_file = temp.path().join("fake-python.pid");
    let hanging_python = temp.path().join("hanging-python");
    write_executable(
        &hanging_python,
        &format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec sleep 30\n",
            pid_file.display()
        ),
    );
    let environments = EnvironmentManager::with_limits(
        hanging_python,
        config.python_sdk.clone(),
        temp.path().join("hanging-runtimes"),
        Duration::from_secs(1),
        128,
    );

    let started = Instant::now();
    let error = environments.prepare(&hook_dir).await.unwrap_err();
    assert!(
        matches!(error, PythonError::EnvironmentTimeout { .. }),
        "{error}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_process_exits(&pid_file).await;

    let noisy_python = temp.path().join("noisy-python");
    write_executable(
        &noisy_python,
        "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 1024 ]; do printf x; i=$((i + 1)); done\nexit 1\n",
    );
    let environments = EnvironmentManager::with_limits(
        noisy_python,
        config.python_sdk.clone(),
        temp.path().join("noisy-runtimes"),
        Duration::from_secs(2),
        64,
    );
    assert!(matches!(
        environments.prepare(&hook_dir).await,
        Err(PythonError::EnvironmentOutputTooLarge {
            stream: "stdout",
            limit: 64,
            ..
        })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn infinite_import_times_out_kills_worker_and_preserves_last_valid_revision() {
    let temp = TempDir::new().unwrap();
    let mut config = config(&temp);
    config.candidate_timeout = Duration::from_secs(1);
    config.paths.create_all().unwrap();
    write_hook(
        &config.paths.hooks,
        "import-timeout",
        "from warden import hook, HookEventKind\n@hook(on=HookEventKind.USER_PROMPT_SUBMITTED)\ndef run(event): return 'valid'\n",
    );
    let hook_dir = config.paths.hooks.join("import-timeout");
    EnvironmentManager::with_limits(
        config.python.clone(),
        config.python_sdk.clone(),
        config.paths.runtimes.clone(),
        Duration::from_secs(20),
        config.max_hook_message_bytes,
    )
    .prepare(&hook_dir)
    .await
    .unwrap();

    let python = Arc::new(PythonRuntime::new(&config));
    let registry = HookRegistry::new(
        config.paths.hooks.clone(),
        config.paths.modules.clone(),
        config.paths.generated_skills.clone(),
        config.paths.runtimes.clone(),
        python.clone(),
    );
    registry.refresh().await.unwrap();
    let id = HookId::parse("import-timeout").unwrap();
    let valid = registry.current(&id).await.unwrap();
    let pid_file = temp.path().join("import-worker.pid");
    write_hook(
        &config.paths.hooks,
        "import-timeout",
        &format!(
            "import os, pathlib, time\npathlib.Path({:?}).write_text(str(os.getpid()))\ntime.sleep(30)\nfrom warden import hook, HookEventKind\n@hook(on=HookEventKind.USER_PROMPT_SUBMITTED)\ndef run(event): return 'unreachable'\n",
            pid_file.to_string_lossy()
        ),
    );

    let started = Instant::now();
    let delta = registry.refresh().await.unwrap();
    assert_eq!(delta.failed.len(), 1);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(
        registry.current(&id).await.unwrap().revision,
        valid.revision
    );
    assert_process_exits(&pid_file).await;
    python.shutdown().await;
}

#[tokio::test]
async fn timeout_kills_worker_and_does_not_poison_replacement_revision() {
    let temp = TempDir::new().unwrap();
    let mut config = config(&temp);
    config.hook_timeout = Duration::from_millis(100);
    config.paths.create_all().unwrap();
    write_hook(
        &config.paths.hooks,
        "slow",
        "import time\nfrom warden import hook, HookEventKind\n@hook(on=HookEventKind.USER_PROMPT_SUBMITTED)\ndef run(event):\n    time.sleep(5)\n",
    );
    let python = Arc::new(PythonRuntime::new(&config));
    let registry = HookRegistry::new(
        config.paths.hooks.clone(),
        config.paths.modules.clone(),
        config.paths.generated_skills.clone(),
        config.paths.runtimes.clone(),
        python.clone(),
    );
    registry.refresh().await.unwrap();
    let id = HookId::parse("slow").unwrap();
    let slow = registry.current(&id).await.unwrap();
    assert!(matches!(
        python.invoke(slow, event(1), credential(&config)).await,
        Err(PythonError::Timeout(_))
    ));

    write_hook(
        &config.paths.hooks,
        "slow",
        "from warden import hook, HookEventKind\n@hook(on=HookEventKind.USER_PROMPT_SUBMITTED)\ndef run(event): return 'healthy'\n",
    );
    registry.refresh().await.unwrap();
    let healthy = registry.current(&id).await.unwrap();
    assert_eq!(
        python
            .invoke(healthy, event(2), credential(&config))
            .await
            .unwrap()
            .result,
        Value::String("healthy".into())
    );
    python.shutdown().await;
}

#[tokio::test]
async fn crash_malformed_output_exception_and_size_failure_are_isolated() {
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    config.paths.create_all().unwrap();
    let cases = [
        ("crash", "import os\nos._exit(3)"),
        (
            "malformed",
            "import os\nos.write(1, b'not-json\\n')\nreturn None",
        ),
        ("exception", "raise RuntimeError('boom')"),
        ("oversized", "return 'x' * 400000"),
    ];
    for (name, body) in cases {
        let indented = body
            .lines()
            .map(|line| format!("    {line}\n"))
            .collect::<String>();
        write_hook(
            &config.paths.hooks,
            name,
            &format!(
                "from warden import hook, HookEventKind\n@hook(on=HookEventKind.USER_PROMPT_SUBMITTED)\ndef run(event):\n{indented}"
            ),
        );
    }
    write_hook(
        &config.paths.hooks,
        "healthy",
        "from warden import hook, HookEventKind\n@hook(on=HookEventKind.USER_PROMPT_SUBMITTED)\ndef run(event): return 'ok'\n",
    );
    let python = Arc::new(PythonRuntime::new(&config));
    let registry = HookRegistry::new(
        config.paths.hooks.clone(),
        config.paths.modules.clone(),
        config.paths.generated_skills.clone(),
        config.paths.runtimes.clone(),
        python.clone(),
    );
    let delta = registry.refresh().await.unwrap();
    assert!(delta.failed.is_empty(), "{:#?}", delta.failed);
    for (index, (name, _)) in cases.into_iter().enumerate() {
        let revision = registry
            .current(&HookId::parse(name).unwrap())
            .await
            .unwrap();
        assert!(
            python
                .invoke(revision, event(index as u64 + 1), credential(&config))
                .await
                .is_err(),
            "{name} should fail only its own invocation"
        );
    }
    let healthy = registry
        .current(&HookId::parse("healthy").unwrap())
        .await
        .unwrap();
    assert_eq!(
        python
            .invoke(healthy, event(10), credential(&config))
            .await
            .unwrap()
            .result,
        Value::String("ok".into())
    );
    python.shutdown().await;
}

#[tokio::test]
async fn hook_capacity_applies_backpressure_instead_of_dropping_deliveries() {
    let temp = TempDir::new().unwrap();
    let mut config = config(&temp);
    config.max_concurrent_hooks = 1;
    config.paths.create_all().unwrap();
    write_hook(
        &config.paths.hooks,
        "queued",
        "import time\nfrom warden import hook, HookEventKind\n@hook(on=HookEventKind.USER_PROMPT_SUBMITTED)\ndef run(event):\n    time.sleep(0.05)\n    return event.sequence\n",
    );
    let python = Arc::new(PythonRuntime::new(&config));
    let registry = HookRegistry::new(
        config.paths.hooks.clone(),
        config.paths.modules.clone(),
        config.paths.generated_skills.clone(),
        config.paths.runtimes.clone(),
        python.clone(),
    );
    registry.refresh().await.unwrap();
    let revision = registry
        .current(&HookId::parse("queued").unwrap())
        .await
        .unwrap();

    let first_runtime = python.clone();
    let first_revision = revision.clone();
    let first_credential = credential(&config);
    let first = tokio::spawn(async move {
        first_runtime
            .invoke(first_revision, event(1), first_credential)
            .await
    });
    tokio::task::yield_now().await;
    let second = python
        .invoke(revision, event(2), credential(&config))
        .await
        .expect("second delivery waits for capacity");
    let first = first.await.unwrap().expect("first delivery succeeds");
    assert_eq!(first.result, json!(1));
    assert_eq!(second.result, json!(2));
    python.shutdown().await;
}
