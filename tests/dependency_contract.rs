//! Compile-time guard for the exact `codex-control` surface Warden consumes.

use codex_control::{Handle, LifecycleItem, ReplayResult};
use serde_json::json;

#[allow(dead_code)]
async fn host_dependency_contract(handle: Handle) {
    let mut lifecycle = handle.lifecycle(0);
    let _lifecycle_item: LifecycleItem = lifecycle.recv().await;
    let _ = handle.deltas();
    let replay: ReplayResult = handle.events_since(0).await;
    let _ = replay;
    let _snapshot = handle.snapshot().await;
    let _retained = handle.query_sequence(Some("thread"), 0, None).await;
    let _ = handle
        .set_skill_extra_roots([std::path::PathBuf::from("/tmp/warden-skills")])
        .await;
    let _ = handle
        .force_refresh_skills([std::path::PathBuf::from("/tmp/project")])
        .await;
    let _start = handle
        .start("thread", vec![json!({"type":"text","text":"start"})])
        .await;
    let _steer = handle
        .steer(
            "thread",
            "turn",
            vec![json!({"type":"text","text":"steer"})],
        )
        .await;
    let _interrupt = handle.interrupt("thread", "turn").await;
}

#[test]
fn pinned_dependency_exposes_the_host_contract() {
    let _ = host_dependency_contract;
}
