use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataPaths {
    pub root: PathBuf,
    pub hooks: PathBuf,
    pub modules: PathBuf,
    pub generated_skills: PathBuf,
    pub runtimes: PathBuf,
    pub sessions: PathBuf,
    pub installations: PathBuf,
    pub native_hooks: PathBuf,
    pub bridge_credential: PathBuf,
    pub action_socket: PathBuf,
}

impl DataPaths {
    pub fn under(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            hooks: root.join("warden-hooks"),
            modules: root.join("modules"),
            generated_skills: root.join("generated-skills"),
            runtimes: root.join("runtimes"),
            sessions: root.join("sessions"),
            installations: root.join("installations"),
            native_hooks: root.join("native-hooks"),
            bridge_credential: root.join("bridge-auth"),
            action_socket: root.join("warden.sock"),
            root,
        }
    }

    pub fn create_all(&self) -> io::Result<()> {
        for path in [
            &self.root,
            &self.hooks,
            &self.modules,
            &self.generated_skills,
            &self.runtimes,
            &self.sessions,
            &self.installations,
            &self.native_hooks,
        ] {
            std::fs::create_dir_all(path)?;
        }
        Ok(())
    }

    pub fn is_generated_skill(&self, path: &Path) -> bool {
        path.starts_with(&self.generated_skills)
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub paths: DataPaths,
    pub python: PathBuf,
    pub python_sdk: PathBuf,
    pub codex_home: PathBuf,
    pub hook_timeout: Duration,
    pub candidate_timeout: Duration,
    pub agent_timeout: Duration,
    pub max_hook_message_bytes: usize,
    pub max_concurrent_hooks: usize,
    pub manage_gui: bool,
}

impl Default for Config {
    fn default() -> Self {
        let root = std::env::var_os("WARDEN_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".warden")))
            .unwrap_or_else(|| PathBuf::from(".warden"));
        Self {
            paths: DataPaths::under(root),
            python: std::env::var_os("WARDEN_PYTHON")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("python3")),
            python_sdk: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python"),
            codex_home: std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
                .unwrap_or_else(|| PathBuf::from(".codex")),
            // Agent-backed hooks must outlive the provider's own bounded deadline so the
            // gateway can return (or cancel) the provider result before the worker is reaped.
            hook_timeout: Duration::from_secs(10 * 60 + 30),
            candidate_timeout: Duration::from_secs(60),
            agent_timeout: Duration::from_secs(10 * 60),
            max_hook_message_bytes: 1024 * 1024,
            max_concurrent_hooks: 16,
            // Restarting the GUI is process-destructive when Warden itself was launched from
            // Codex Desktop. Library callers must opt in just like the daemon CLI does.
            manage_gui: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_separate_and_skill_check_is_scoped() {
        let paths = DataPaths::under("/tmp/warden-test-root");
        assert_ne!(paths.hooks, paths.generated_skills);
        assert!(paths.is_generated_skill(&paths.generated_skills.join("demo/SKILL.md")));
        assert!(!paths.is_generated_skill(&paths.hooks.join("demo/hook.py")));
    }

    #[test]
    fn default_config_never_manages_codex_desktop() {
        assert!(!Config::default().manage_gui);
    }
}
