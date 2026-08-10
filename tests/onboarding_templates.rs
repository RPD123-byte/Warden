use std::{collections::HashSet, path::Path, sync::Arc};

use tempfile::TempDir;
use warden_daemon::{
    Config, DataPaths, HookId, HookRegistry, MARKER_BODY, event::HookEventKind,
    python::PythonRuntime, reconcile_codex,
};

fn config(temp: &TempDir) -> Config {
    Config {
        paths: DataPaths::under(temp.path().join("custom-warden-home")),
        codex_home: temp.path().join("codex"),
        agents_home: temp.path().join("agents"),
        python_sdk: Path::new(env!("CARGO_MANIFEST_DIR")).join("python"),
        ..Config::default()
    }
}

#[tokio::test]
async fn installed_template_validates_and_generates_its_exact_marker() {
    let temp = TempDir::new().unwrap();
    let config = config(&temp);
    let onboarding = reconcile_codex(&config).unwrap();
    assert!(onboarding.hook_templates[0].installed);

    let python = Arc::new(PythonRuntime::new(&config));
    let registry = HookRegistry::new(
        config.paths.hooks.clone(),
        config.paths.modules.clone(),
        config.paths.generated_skills.clone(),
        config.paths.runtimes.clone(),
        python,
    );
    let delta = registry.refresh().await.unwrap();
    let id = HookId::parse("unspecified-decisions").unwrap();
    assert_eq!(delta.published, vec![id.clone()]);
    let revision = registry.current(&id).await.unwrap();

    assert!(revision.metadata.blocking);
    assert!(revision.metadata.persistent_agent_sessions);
    assert_eq!(
        revision.metadata.events,
        HashSet::from([
            HookEventKind::PostToolUse,
            HookEventKind::PostToolUseFailure,
            HookEventKind::AgentMessageCompleted,
        ])
    );
    assert_eq!(
        revision.metadata.actions,
        HashSet::from([
            "current_thread_history".to_owned(),
            "turn_interrupt".to_owned(),
            "turn_start".to_owned(),
        ])
    );
    let marker = std::fs::read_to_string(
        config
            .paths
            .generated_skills
            .join("unspecified-decisions/SKILL.md"),
    )
    .unwrap();
    assert!(marker.trim().ends_with(MARKER_BODY));
    for control in ["start", "pause", "resume", "stop"] {
        let marker = std::fs::read_to_string(
            config
                .paths
                .generated_skills
                .join(format!("unspecified-decisions-{control}/SKILL.md")),
        )
        .unwrap();
        assert!(marker.trim().ends_with(MARKER_BODY));
    }

    let source =
        std::fs::read_to_string(config.paths.hooks.join("unspecified-decisions/hook.py")).unwrap();
    assert!(source.contains("only user-authored instructions and governing specifications"));
    assert!(source.contains("only an\nactivation marker"));
    assert!(source.contains("Agent commentary, plans, tool calls, tool results"));
    assert!(source.contains("Silence and a previous no-action verdict are not approval"));
    assert!(source.contains("queued steering is\n   discarded"));
    assert!(source.contains("\"unspecified-decision-monitor-v4\""));
}
