use crate::{Config, native_hook::NativeHookInstall};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
};

const AUTHORING_SKILL: &str = include_str!("../.agents/skills/create-warden-hook/SKILL.md");
const UNSPECIFIED_DECISIONS_HOOK: &str =
    include_str!("../.warden/warden-hooks/unspecified-decisions/hook.py");

struct EmbeddedTemplateFile {
    path: &'static str,
    contents: &'static [u8],
}

struct EmbeddedHookTemplate {
    name: &'static str,
    files: &'static [EmbeddedTemplateFile],
}

const UNSPECIFIED_DECISIONS_FILES: &[EmbeddedTemplateFile] = &[EmbeddedTemplateFile {
    path: "hook.py",
    contents: UNSPECIFIED_DECISIONS_HOOK.as_bytes(),
}];

const HOOK_TEMPLATES: &[EmbeddedHookTemplate] = &[EmbeddedHookTemplate {
    name: "unspecified-decisions",
    files: UNSPECIFIED_DECISIONS_FILES,
}];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookTemplateInstall {
    pub name: String,
    pub path: PathBuf,
    pub installed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexOnboarding {
    pub authoring_skill: PathBuf,
    pub agents_authoring_skill: PathBuf,
    pub skill_changed: bool,
    pub hook_templates: Vec<HookTemplateInstall>,
    pub native_hooks: NativeHookInstall,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManagedSkillManifest {
    target: PathBuf,
    installed_hash: String,
}

pub fn reconcile_codex(config: &Config) -> io::Result<CodexOnboarding> {
    config.paths.create_all()?;
    let hook_templates = install_hook_templates(config)?;
    let (authoring_skill, codex_skill_changed) = install_authoring_skill(
        &config.codex_home.join("skills/create-warden-hook/SKILL.md"),
        &config.paths.installations.join("create-warden-hook.json"),
        "Codex",
    )?;
    let (agents_authoring_skill, agents_skill_changed) = install_authoring_skill(
        &config
            .agents_home
            .join("skills/create-warden-hook/SKILL.md"),
        &config
            .paths
            .installations
            .join("create-warden-hook-agents.json"),
        "Agents",
    )?;
    let native_hooks = crate::native_hook::ensure_native_bridge_bundle(config)?;
    Ok(CodexOnboarding {
        authoring_skill,
        agents_authoring_skill,
        skill_changed: codex_skill_changed || agents_skill_changed,
        hook_templates,
        native_hooks,
    })
}

fn install_hook_templates(config: &Config) -> io::Result<Vec<HookTemplateInstall>> {
    HOOK_TEMPLATES
        .iter()
        .map(|template| {
            let path = config.paths.hooks.join(template.name);
            let installed = install_hook_template(template, &path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "could not install Warden hook template {:?} at {}: {error}",
                        template.name,
                        path.display()
                    ),
                )
            })?;
            Ok(HookTemplateInstall {
                name: template.name.to_owned(),
                path,
                installed,
            })
        })
        .collect()
}

fn install_hook_template(template: &EmbeddedHookTemplate, destination: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(destination) {
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "template has no parent"))?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".warden-template-{}-{}",
        template.name,
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        fs::create_dir(&staging)?;
        for file in template.files {
            let target = staging.join(file.path);
            let target_parent = target
                .parent()
                .expect("embedded template file has a parent");
            fs::create_dir_all(target_parent)?;
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&target)?;
            output.write_all(file.contents)?;
            output.sync_all()?;
        }
        sync_directory(&staging)?;
        match rustix::fs::renameat_with(
            rustix::fs::CWD,
            &staging,
            rustix::fs::CWD,
            destination,
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {
                sync_directory(parent)?;
                Ok(true)
            }
            Err(error) if error == rustix::io::Errno::EXIST => Ok(false),
            Err(error) => Err(io::Error::from_raw_os_error(error.raw_os_error())),
        }
    })();
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_: &Path) -> io::Result<()> {
    Ok(())
}

fn install_authoring_skill(
    target: &Path,
    manifest_path: &Path,
    location_name: &str,
) -> io::Result<(PathBuf, bool)> {
    let wanted_hash = hash(AUTHORING_SKILL.as_bytes());
    let existing = fs::read(target).ok();
    if existing.as_deref() == Some(AUTHORING_SKILL.as_bytes()) {
        ensure_manifest(manifest_path, target, &wanted_hash)?;
        return Ok((target.to_owned(), false));
    }
    if let Some(existing) = existing {
        let owned = fs::read(manifest_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ManagedSkillManifest>(&bytes).ok())
            .is_some_and(|manifest| {
                manifest.target == target && manifest.installed_hash == hash(&existing)
            });
        if !owned {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "refusing to overwrite unmanaged {location_name} skill {}",
                    target.display()
                ),
            ));
        }
    }
    atomic_write(target, AUTHORING_SKILL.as_bytes())?;
    ensure_manifest(manifest_path, target, &wanted_hash)?;
    Ok((target.to_owned(), true))
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
            agents_home: temp.path().join("agents"),
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
        assert_eq!(
            fs::read_to_string(&first.agents_authoring_skill).unwrap(),
            AUTHORING_SKILL
        );
        assert!(first.native_hooks.changed);
        assert_eq!(first.hook_templates.len(), 1);
        assert!(first.hook_templates[0].installed);
        assert_eq!(first.hook_templates[0].name, "unspecified-decisions");
        assert_eq!(
            fs::read_to_string(config.paths.hooks.join("unspecified-decisions/hook.py")).unwrap(),
            UNSPECIFIED_DECISIONS_HOOK
        );
        let second = reconcile_codex(&config).unwrap();
        assert!(!second.skill_changed);
        assert_eq!(second.agents_authoring_skill, first.agents_authoring_skill);
        assert!(!second.native_hooks.changed);
        assert!(!second.hook_templates[0].installed);
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

    #[test]
    fn unmanaged_agents_skill_collision_is_preserved() {
        let temp = TempDir::new().unwrap();
        let config = config(&temp);
        let target = config
            .agents_home
            .join("skills/create-warden-hook/SKILL.md");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "user owned").unwrap();

        let error = reconcile_codex(&config).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(target).unwrap(), "user owned");
    }

    #[test]
    fn existing_template_directory_is_never_merged_or_overwritten() {
        let temp = TempDir::new().unwrap();
        let config = config(&temp);
        let destination = config.paths.hooks.join("unspecified-decisions");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("notes.txt"), "user owned").unwrap();

        let result = reconcile_codex(&config).unwrap();

        assert!(!result.hook_templates[0].installed);
        assert_eq!(
            fs::read_to_string(destination.join("notes.txt")).unwrap(),
            "user owned"
        );
        assert!(!destination.join("hook.py").exists());
    }

    #[test]
    fn deleting_the_template_directory_restores_the_current_embedded_copy() {
        let temp = TempDir::new().unwrap();
        let config = config(&temp);
        let destination = config.paths.hooks.join("unspecified-decisions");
        reconcile_codex(&config).unwrap();
        fs::remove_dir_all(&destination).unwrap();

        let result = reconcile_codex(&config).unwrap();

        assert!(result.hook_templates[0].installed);
        assert_eq!(
            fs::read_to_string(destination.join("hook.py")).unwrap(),
            UNSPECIFIED_DECISIONS_HOOK
        );
    }

    #[test]
    fn failed_staging_is_removed_and_never_published() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("custom-home/warden-hooks");
        fs::create_dir_all(&hooks).unwrap();
        const FILES: &[EmbeddedTemplateFile] = &[
            EmbeddedTemplateFile {
                path: "hook.py",
                contents: b"first",
            },
            EmbeddedTemplateFile {
                path: "hook.py",
                contents: b"duplicate",
            },
        ];
        let template = EmbeddedHookTemplate {
            name: "broken",
            files: FILES,
        };
        let destination = hooks.join("broken");

        let error = install_hook_template(&template, &destination).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!destination.exists());
        assert!(fs::read_dir(&hooks).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".warden-template-")
        }));
    }

    #[test]
    fn template_install_error_names_the_template_and_destination() {
        let temp = TempDir::new().unwrap();
        let mut config = config(&temp);
        config.paths.hooks = temp.path().join("not-a-directory");
        fs::write(&config.paths.hooks, "occupied").unwrap();

        let error = install_hook_templates(&config).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("unspecified-decisions"), "{message}");
        assert!(message.contains("not-a-directory"), "{message}");
    }
}
