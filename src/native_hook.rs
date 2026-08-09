use crate::config::Config;
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

const BRIDGE_SOURCE: &str = include_str!("../python/warden_native_bridge.py");
const STATUS_PREFIX: &str = "Warden bridge: ";
const LEGACY_STATUS: &str = "Waiting for Warden blocking hooks";
const BRIDGE_TIMEOUT_SECONDS: u64 = 630;
const BRIDGE_SOCKET_TIMEOUT_SECONDS: u64 = 605;
pub const BRIDGE_EVENTS: [&str; 4] = ["UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeHookInstall {
    pub hooks_file: PathBuf,
    pub bridge_script: PathBuf,
    pub credential_file: PathBuf,
    pub command: String,
    pub changed: bool,
}

pub fn ensure_native_bridge_bundle(config: &Config) -> io::Result<NativeHookInstall> {
    config.paths.create_all()?;
    fs::create_dir_all(&config.codex_home)?;
    let bridge_script = config.paths.native_hooks.join("bridge.py");
    let script_changed = write_if_changed(&bridge_script, BRIDGE_SOURCE.as_bytes())?;
    set_owner_only(&bridge_script, 0o700)?;
    ensure_credential(&config.paths.bridge_credential)?;

    let hooks_file = config.codex_home.join("hooks.json");
    let command = format!(
        "python3 {} --socket {} --credential-file {} --timeout {}",
        shell_quote(&bridge_script),
        shell_quote(&config.paths.action_socket),
        shell_quote(&config.paths.bridge_credential),
        BRIDGE_SOCKET_TIMEOUT_SECONDS,
    );
    let mut document = read_hooks_document(&hooks_file)?;
    let original = document.clone();
    let hooks = document
        .as_object_mut()
        .ok_or_else(|| invalid("hooks.json root must be an object"))?
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| invalid("hooks must be an object"))?;

    for event_name in BRIDGE_EVENTS {
        reconcile_event(hooks, event_name, &command)?;
    }
    let config_changed = document != original || !hooks_file.is_file();
    if config_changed {
        let mut bytes = serde_json::to_vec_pretty(&document).map_err(io::Error::other)?;
        bytes.push(b'\n');
        atomic_write(&hooks_file, &bytes)?;
    }

    Ok(NativeHookInstall {
        hooks_file,
        bridge_script,
        credential_file: config.paths.bridge_credential.clone(),
        command,
        changed: script_changed || config_changed,
    })
}

/// Removes only Warden-owned native configuration entries. The bridge program and credential
/// remain in place so already-loaded Codex tasks fail open until they are restarted.
pub fn remove_native_bridge_entries(config: &Config) -> io::Result<bool> {
    let hooks_file = config.codex_home.join("hooks.json");
    if !hooks_file.is_file() {
        return Ok(false);
    }
    let mut document = read_hooks_document(&hooks_file)?;
    let original = document.clone();
    let hooks = document
        .as_object_mut()
        .ok_or_else(|| invalid("hooks.json root must be an object"))?
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid("hooks must be an object"))?;
    for event_name in BRIDGE_EVENTS {
        let Some(groups) = hooks.get_mut(event_name).and_then(Value::as_array_mut) else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                handlers.retain(|candidate| !is_warden_handler(candidate));
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|handlers| !handlers.is_empty())
        });
    }
    if document == original {
        return Ok(false);
    }
    let mut bytes = serde_json::to_vec_pretty(&document).map_err(io::Error::other)?;
    bytes.push(b'\n');
    atomic_write(&hooks_file, &bytes)?;
    Ok(true)
}

fn read_hooks_document(path: &Path) -> io::Result<Value> {
    if !path.is_file() {
        return Ok(json!({"hooks": {}}));
    }
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not valid JSON: {error}", path.display()),
        )
    })
}

fn reconcile_event(
    hooks: &mut Map<String, Value>,
    event_name: &str,
    command: &str,
) -> io::Result<()> {
    let groups = hooks
        .entry(event_name)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| invalid(format!("hooks.{event_name} must be an array")))?;
    let handler = json!({
        "type": "command",
        "command": command,
        "statusMessage": format!("{STATUS_PREFIX}{event_name}"),
        "timeout": BRIDGE_TIMEOUT_SECONDS,
    });
    let mut destination = None;
    for (index, group) in groups.iter_mut().enumerate() {
        let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        let before = handlers.len();
        handlers.retain(|candidate| !is_warden_handler(candidate));
        if handlers.len() != before && destination.is_none() {
            destination = Some(index);
        }
    }
    if let Some(index) = destination {
        groups[index]
            .get_mut("hooks")
            .and_then(Value::as_array_mut)
            .expect("destination was selected from a hooks array")
            .push(handler);
    } else {
        groups.push(json!({"hooks": [handler]}));
    }
    Ok(())
}

fn is_warden_handler(value: &Value) -> bool {
    value
        .get("statusMessage")
        .and_then(Value::as_str)
        .is_some_and(|status| status == LEGACY_STATUS || status.starts_with(STATUS_PREFIX))
}

fn ensure_credential(path: &Path) -> io::Result<()> {
    if path.is_file() {
        if fs::read_to_string(path)?.trim().is_empty() {
            return Err(invalid("bridge credential exists but is empty"));
        }
        return set_owner_only(path, 0o600);
    }
    let token = format!(
        "{}{}\n",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    set_owner_only(path, 0o600)
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    if fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(false);
    }
    atomic_write(path, bytes)?;
    Ok(true)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| invalid("path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.warden-tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file"),
        std::process::id(),
        uuid::Uuid::new_v4().simple(),
    ));
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(unix)]
fn set_owner_only(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_owner_only(_: &Path, _: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, DataPaths};
    use tempfile::TempDir;

    #[test]
    fn installation_preserves_existing_hooks_replaces_legacy_and_is_idempotent() {
        let directory = TempDir::new().unwrap();
        let codex_home = directory.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        fs::write(
            codex_home.join("hooks.json"),
            br#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo existing"}]}],"UserPromptSubmit":[{"hooks":[{"type":"command","command":"dead","statusMessage":"Waiting for Warden blocking hooks"}]}]}}"#,
        )
        .unwrap();
        let config = Config {
            paths: DataPaths::under(directory.path().join("warden")),
            codex_home: codex_home.clone(),
            ..Config::default()
        };

        let first = ensure_native_bridge_bundle(&config).unwrap();
        assert!(first.changed);
        let first_bytes = fs::read(codex_home.join("hooks.json")).unwrap();
        let second = ensure_native_bridge_bundle(&config).unwrap();
        assert!(!second.changed);
        assert_eq!(
            first_bytes,
            fs::read(codex_home.join("hooks.json")).unwrap()
        );

        let document: Value = serde_json::from_slice(&first_bytes).unwrap();
        assert_eq!(
            document["hooks"]["Stop"][0]["hooks"][0]["command"],
            "echo existing"
        );
        for event in BRIDGE_EVENTS {
            let handlers = document["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|group| group["hooks"].as_array().unwrap())
                .filter(|handler| is_warden_handler(handler))
                .count();
            assert_eq!(handlers, 1, "{event}");
        }
        assert!(first.bridge_script.is_file());
        assert!(first.credential_file.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&first.bridge_script)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&first.credential_file)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn malformed_configuration_is_never_replaced() {
        let directory = TempDir::new().unwrap();
        let codex_home = directory.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        fs::write(codex_home.join("hooks.json"), "not json").unwrap();
        let config = Config {
            paths: DataPaths::under(directory.path().join("warden")),
            codex_home: codex_home.clone(),
            ..Config::default()
        };
        assert!(ensure_native_bridge_bundle(&config).is_err());
        assert_eq!(
            fs::read_to_string(codex_home.join("hooks.json")).unwrap(),
            "not json"
        );
    }

    #[test]
    fn removal_deletes_only_warden_entries_and_is_idempotent() {
        let directory = TempDir::new().unwrap();
        let codex_home = directory.path().join("codex");
        let config = Config {
            paths: DataPaths::under(directory.path().join("warden")),
            codex_home: codex_home.clone(),
            ..Config::default()
        };
        ensure_native_bridge_bundle(&config).unwrap();
        let mut document = read_hooks_document(&codex_home.join("hooks.json")).unwrap();
        document["hooks"]["Stop"].as_array_mut().unwrap().insert(
            0,
            json!({"hooks":[{"type":"command","command":"user command"}]}),
        );
        atomic_write(
            &codex_home.join("hooks.json"),
            &serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();

        assert!(remove_native_bridge_entries(&config).unwrap());
        assert!(!remove_native_bridge_entries(&config).unwrap());
        let removed = read_hooks_document(&codex_home.join("hooks.json")).unwrap();
        assert_eq!(
            removed["hooks"]["Stop"][0]["hooks"][0]["command"],
            "user command"
        );
        for event in BRIDGE_EVENTS {
            assert!(
                removed["hooks"][event]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|group| group["hooks"].as_array().unwrap())
                    .all(|handler| !is_warden_handler(handler))
            );
        }
        assert!(config.paths.bridge_credential.is_file());
    }
}
