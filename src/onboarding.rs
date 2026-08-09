use crate::{Config, native_hook::NativeHookInstall};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

const AUTHORING_SKILL: &str = include_str!("../.agents/skills/create-warden-hook/SKILL.md");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexOnboarding {
    pub authoring_skill: PathBuf,
    pub skill_changed: bool,
    pub native_hooks: NativeHookInstall,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManagedSkillManifest {
    target: PathBuf,
    installed_hash: String,
}

pub fn reconcile_codex(config: &Config) -> io::Result<CodexOnboarding> {
    config.paths.create_all()?;
    let (authoring_skill, skill_changed) = install_authoring_skill(config)?;
    let native_hooks = crate::native_hook::ensure_native_bridge_bundle(config)?;
    Ok(CodexOnboarding {
        authoring_skill,
        skill_changed,
        native_hooks,
    })
}

fn install_authoring_skill(config: &Config) -> io::Result<(PathBuf, bool)> {
    let target = config.codex_home.join("skills/create-warden-hook/SKILL.md");
    let manifest_path = config.paths.installations.join("create-warden-hook.json");
    let wanted_hash = hash(AUTHORING_SKILL.as_bytes());
    let existing = fs::read(&target).ok();
    if existing.as_deref() == Some(AUTHORING_SKILL.as_bytes()) {
        ensure_manifest(&manifest_path, &target, &wanted_hash)?;
        return Ok((target, false));
    }
    if let Some(existing) = existing {
        let owned = fs::read(&manifest_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ManagedSkillManifest>(&bytes).ok())
            .is_some_and(|manifest| {
                manifest.target == target && manifest.installed_hash == hash(&existing)
            });
        if !owned {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "refusing to overwrite unmanaged Codex skill {}",
                    target.display()
                ),
            ));
        }
    }
    atomic_write(&target, AUTHORING_SKILL.as_bytes())?;
    ensure_manifest(&manifest_path, &target, &wanted_hash)?;
    Ok((target, true))
}

fn ensure_manifest(path: &Path, target: &Path, installed_hash: &str) -> io::Result<()> {
    let manifest = ManagedSkillManifest {
        target: target.to_owned(),
        installed_hash: installed_hash.to_owned(),
    };
    let mut bytes = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
    bytes.push(b'\n');
    if fs::read(path).ok().as_deref() != Some(bytes.as_slice()) {
        atomic_write(path, &bytes)?;
    }
    Ok(())
}

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".warden-install-{}", uuid::Uuid::new_v4().simple()));
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DataPaths;
    use tempfile::TempDir;

    fn config(temp: &TempDir) -> Config {
        Config {
            paths: DataPaths::under(temp.path().join("warden")),
            codex_home: temp.path().join("codex"),
            ..Config::default()
        }
    }

    #[test]
    fn first_and_repeated_onboarding_are_idempotent() {
        let temp = TempDir::new().unwrap();
        let config = config(&temp);
        let unrelated_skill = config.codex_home.join("skills/user-skill/SKILL.md");
        fs::create_dir_all(unrelated_skill.parent().unwrap()).unwrap();
        fs::write(&unrelated_skill, "user owned skill").unwrap();
        fs::create_dir_all(&config.codex_home).unwrap();
        fs::write(
            config.codex_home.join("hooks.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"user command"}]}]}}"#,
        )
        .unwrap();
        let first = reconcile_codex(&config).unwrap();
        assert!(first.skill_changed);
        assert!(first.native_hooks.changed);
        assert!(!config.paths.hooks.join("example/hook.py").exists());
        let second = reconcile_codex(&config).unwrap();
        assert!(!second.skill_changed);
        assert!(!second.native_hooks.changed);
        assert_eq!(
            fs::read_to_string(unrelated_skill).unwrap(),
            "user owned skill"
        );
        let hooks: serde_json::Value =
            serde_json::from_slice(&fs::read(config.codex_home.join("hooks.json")).unwrap())
                .unwrap();
        assert_eq!(
            hooks["hooks"]["Stop"][0]["hooks"][0]["command"],
            "user command"
        );
    }

    #[test]
    fn unmanaged_skill_collision_is_preserved() {
        let temp = TempDir::new().unwrap();
        let config = config(&temp);
        let target = config.codex_home.join("skills/create-warden-hook/SKILL.md");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "user owned").unwrap();
        let error = reconcile_codex(&config).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(target).unwrap(), "user owned");
    }
}
