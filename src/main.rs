use clap::Parser;
use std::path::{Path, PathBuf};
use warden_daemon::{Config, DataPaths, Warden, remove_native_bridge_entries};

#[derive(Debug, Parser)]
#[command(
    name = "warden-daemon",
    version,
    about = "Turn-scoped local hooks for Codex tasks"
)]
struct Arguments {
    /// Root for authored hooks, generated marker skills, runtimes, and sessions.
    #[arg(long, env = "WARDEN_HOME")]
    home: Option<PathBuf>,

    /// Python 3.11+ interpreter used to create isolated hook environments.
    #[arg(long, env = "WARDEN_PYTHON")]
    python: Option<PathBuf>,

    /// Opt in to quitting/restarting Codex Desktop so it attaches to the shared app-server.
    /// Run this only from an independent terminal or service, never from Codex Desktop itself.
    #[arg(long)]
    manage_gui: bool,

    /// Remove only Warden-owned Codex native bridge entries, then exit.
    #[arg(long, conflicts_with = "manage_gui")]
    remove_native_bridges: bool,
}

impl Arguments {
    fn into_config(self) -> Config {
        let mut config = Config::default();
        if let Some(home) = self.home {
            config.paths = DataPaths::under(home);
        }
        if let Some(python) = self.python {
            config.python = python;
        }
        config.manage_gui = self.manage_gui;
        config
    }

    fn validate_manage_gui_ancestry(&self, ancestors: &[PathBuf]) -> anyhow::Result<()> {
        if !self.manage_gui {
            return Ok(());
        }
        let Some(owner) = ancestors.iter().find(|path| is_codex_desktop_path(path)) else {
            return Ok(());
        };
        anyhow::bail!(
            "refusing --manage-gui because Warden is running under Codex Desktop ({owner}); \
             run this command from an independent macOS Terminal window or background service",
            owner = owner.display()
        )
    }

    fn validate_invocation(&self) -> anyhow::Result<()> {
        if !self.manage_gui {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            let ancestors = process_ancestry().map_err(|error| {
                anyhow::anyhow!(
                    "refusing --manage-gui because Warden could not safely inspect its process \
                     ancestry: {error}; run this command from an independent macOS Terminal \
                     window or background service"
                )
            })?;
            self.validate_manage_gui_ancestry(&ancestors)?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let arguments = Arguments::parse();
    arguments.validate_invocation()?;
    let remove = arguments.remove_native_bridges;
    let config = arguments.into_config();
    if remove {
        let changed = remove_native_bridge_entries(&config)?;
        println!(
            "{}",
            if changed {
                "Removed Warden-owned Codex native bridge entries. Restart existing Codex tasks to unload already-captured bridges."
            } else {
                "No Warden-owned Codex native bridge entries were present."
            }
        );
        return Ok(());
    }
    Warden::run(config).await?;
    Ok(())
}

fn is_codex_desktop_path(path: &Path) -> bool {
    [
        Path::new("/Applications/ChatGPT.app"),
        Path::new("/Applications/Codex.app"),
    ]
    .iter()
    .any(|bundle| path == *bundle || path.starts_with(bundle))
}

#[cfg(target_os = "macos")]
fn process_ancestry() -> anyhow::Result<Vec<PathBuf>> {
    let mut pid = std::process::id();
    let mut ancestors = Vec::new();
    for _ in 0..64 {
        let output = std::process::Command::new("/bin/ps")
            .args(["-ww", "-o", "ppid=", "-o", "comm=", "-p", &pid.to_string()])
            .output()?;
        if !output.status.success() {
            anyhow::bail!("/bin/ps failed while inspecting process {pid}");
        }
        let record = String::from_utf8(output.stdout)?;
        let record = record.trim();
        if record.is_empty() {
            anyhow::bail!("process {pid} disappeared during ancestry inspection");
        }
        let mut fields = record.splitn(2, char::is_whitespace);
        let parent: u32 = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("process {pid} has no parent field"))?
            .parse()?;
        let command = fields
            .next()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .ok_or_else(|| anyhow::anyhow!("process {pid} has no command field"))?;
        ancestors.push(PathBuf::from(command));
        if parent <= 1 {
            return Ok(ancestors);
        }
        if parent == pid {
            anyhow::bail!("process {pid} is its own parent");
        }
        pid = parent;
    }
    anyhow::bail!("process ancestry exceeded 64 generations")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_startup_never_manages_or_restarts_codex_desktop() {
        let arguments = Arguments::try_parse_from(["warden-daemon"]).unwrap();
        let config = arguments.into_config();

        assert!(
            !config.manage_gui,
            "plain `warden-daemon` must attach without quitting/restarting Codex Desktop"
        );
    }

    #[test]
    fn desktop_management_requires_an_explicit_flag() {
        let arguments = Arguments::try_parse_from(["warden-daemon", "--manage-gui"]).unwrap();

        assert!(arguments.into_config().manage_gui);
    }

    #[test]
    fn explicit_management_is_refused_under_chatgpt_or_codex_desktop() {
        let arguments = Arguments::try_parse_from(["warden-daemon", "--manage-gui"]).unwrap();
        for owner in [
            "/Applications/ChatGPT.app/Contents/Resources/codex",
            "/Applications/Codex.app/Contents/MacOS/Codex",
        ] {
            let error = arguments
                .validate_manage_gui_ancestry(&[PathBuf::from("/bin/zsh"), PathBuf::from(owner)])
                .expect_err("a desktop-owned daemon cannot safely restart its owner");
            let message = error.to_string();
            assert!(message.contains("refusing --manage-gui"), "{message}");
            assert!(message.contains("independent macOS Terminal"), "{message}");
        }
    }

    #[test]
    fn attach_only_and_independent_terminal_management_pass_preflight() {
        let desktop_ancestry = [PathBuf::from(
            "/Applications/ChatGPT.app/Contents/Resources/codex",
        )];
        Arguments::try_parse_from(["warden-daemon"])
            .unwrap()
            .validate_manage_gui_ancestry(&desktop_ancestry)
            .unwrap();

        Arguments::try_parse_from(["warden-daemon", "--manage-gui"])
            .unwrap()
            .validate_manage_gui_ancestry(&[
                PathBuf::from("/usr/bin/zsh"),
                PathBuf::from("/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"),
            ])
            .unwrap();
    }
}
