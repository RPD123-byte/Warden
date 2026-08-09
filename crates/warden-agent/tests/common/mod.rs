#![allow(dead_code)]

use std::{fs, path::Path, time::Duration};

use tempfile::TempDir;
use warden_agent::CliConfig;

pub fn write_script(directory: &TempDir, name: &str, source: &str) -> std::path::PathBuf {
    let path = directory.path().join(name);
    fs::write(&path, source).expect("write fake provider script");
    path
}

pub fn shell_config(script: &Path, timeout: Duration) -> CliConfig {
    CliConfig::new("/bin/sh")
        .with_prefix_arg(script.as_os_str())
        .with_timeout(timeout)
}
