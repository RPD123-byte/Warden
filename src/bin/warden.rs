use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{os::unix::process::CommandExt, path::PathBuf, process::ExitCode};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use uuid::Uuid;
use warden_daemon::action::{ACTION_PROTOCOL_VERSION, GatewayResponse};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(name = "warden", version, about = "Client for the local Warden daemon")]
struct Arguments {
    #[arg(long, env = "WARDEN_SOCKET")]
    socket: Option<PathBuf>,

    #[arg(long, env = "WARDEN_INVOCATION_ID", hide_env_values = true)]
    invocation_id: Option<Uuid>,

    #[arg(long, env = "WARDEN_INVOCATION_AUTH", hide_env_values = true)]
    token: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start Warden in the foreground and perform idempotent Codex onboarding.
    Start {
        /// Root for authored hooks and Warden-managed state.
        #[arg(long, env = "WARDEN_HOME")]
        home: Option<PathBuf>,
        /// Python interpreter used for isolated hook environments.
        #[arg(long, env = "WARDEN_PYTHON")]
        python: Option<PathBuf>,
        /// Explicitly allow Warden to restart Codex Desktop during startup.
        #[arg(long)]
        manage_gui: bool,
    },
    /// Report transport, hook-registry, continuous-session, and lifecycle coverage health.
    Health,
    /// Remove only Warden-owned Codex native bridge entries.
    RemoveNativeBridges {
        /// Root for authored hooks and Warden-managed state.
        #[arg(long, env = "WARDEN_HOME")]
        home: Option<PathBuf>,
    },
    /// Execute one catalog action. Invocation credentials are normally inherited.
    Action {
        /// Action wire name, for example current_event or turn_interrupt.
        name: String,
        /// JSON object with action arguments.
        #[arg(long, default_value = "{}")]
        arguments: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match execute(Arguments::parse()).await {
        Ok(Some(value)) => {
            match serde_json::to_string_pretty(&value) {
                Ok(output) => println!("{output}"),
                Err(error) => {
                    eprintln!("could not encode Warden response: {error}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("warden: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(arguments: Arguments) -> Result<Option<Value>, String> {
    let socket = arguments.socket.unwrap_or_else(default_socket);
    let (method, params, needs_credential) = match arguments.command {
        Command::Start {
            home,
            python,
            manage_gui,
        } => {
            run_daemon(home, python, manage_gui, false)?;
            return Ok(None);
        }
        Command::Health => ("warden.health", json!({}), false),
        Command::RemoveNativeBridges { home } => {
            run_daemon(home, None, false, true)?;
            return Ok(None);
        }
        Command::Action { name, arguments } => {
            let arguments: Value = serde_json::from_str(&arguments)
                .map_err(|error| format!("--arguments is not valid JSON: {error}"))?;
            if !arguments.is_object() {
                return Err("--arguments must be a JSON object".into());
            }
            (
                "warden.action",
                json!({"name": name, "arguments": arguments}),
                true,
            )
        }
    };
    if needs_credential && (arguments.invocation_id.is_none() || arguments.token.is_none()) {
        return Err(
            "this action requires WARDEN_INVOCATION_ID and WARDEN_INVOCATION_AUTH from an active hook"
                .into(),
        );
    }
    let id = Uuid::new_v4().simple().to_string();
    let request = json!({
        "type": "request",
        "protocol_version": ACTION_PROTOCOL_VERSION,
        "id": id,
        "method": method,
        "params": params,
        "context": match (arguments.invocation_id, arguments.token) {
            (Some(invocation_id), Some(token)) => json!({"invocation_id": invocation_id, "token": token}),
            _ => Value::Null,
        },
    });
    let mut encoded = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    encoded.push(b'\n');
    let stream = UnixStream::connect(&socket)
        .await
        .map_err(|error| format!("could not connect to {}: {error}", socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(&encoded)
        .await
        .map_err(|error| format!("could not send request: {error}"))?;
    let mut reader = BufReader::new(reader).take((MAX_RESPONSE_BYTES + 1) as u64);
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .await
        .map_err(|error| format!("could not read response: {error}"))?;
    if line.is_empty() || line.len() > MAX_RESPONSE_BYTES || !line.ends_with(b"\n") {
        return Err("daemon returned an empty, oversized, or unterminated response".into());
    }
    let response: GatewayResponse = serde_json::from_slice(&line)
        .map_err(|error| format!("invalid daemon response: {error}"))?;
    if response.message_type != "response" || response.protocol_version != ACTION_PROTOCOL_VERSION {
        return Err("daemon response used an unsupported protocol".into());
    }
    if response.id != id {
        return Err("daemon response id did not match request".into());
    }
    if response.ok {
        Ok(Some(response.result.unwrap_or(Value::Null)))
    } else {
        let error = response
            .error
            .ok_or_else(|| "daemon rejected request without an error".to_owned())?;
        Err(format!("{}: {}", error.code, error.message))
    }
}

fn run_daemon(
    home: Option<PathBuf>,
    python: Option<PathBuf>,
    manage_gui: bool,
    remove_native_bridges: bool,
) -> Result<(), String> {
    let mut command = std::process::Command::new(daemon_binary());
    if let Some(home) = home {
        command.arg("--home").arg(home);
    }
    if let Some(python) = python {
        command.arg("--python").arg(python);
    }
    if manage_gui {
        command.arg("--manage-gui");
    }
    if remove_native_bridges {
        command.arg("--remove-native-bridges");
    }
    let error = command.exec();
    Err(format!("could not start warden-daemon: {error}"))
}

fn daemon_binary() -> PathBuf {
    if let Ok(current) = std::env::current_exe()
        && let Some(parent) = current.parent()
    {
        let sibling = parent.join(if cfg!(windows) {
            "warden-daemon.exe"
        } else {
            "warden-daemon"
        });
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("warden-daemon")
}

fn default_socket() -> PathBuf {
    std::env::var_os("WARDEN_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".warden")))
        .unwrap_or_else(|| PathBuf::from(".warden"))
        .join("warden.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_is_the_primary_daemon_command() {
        let arguments = Arguments::try_parse_from([
            "warden",
            "start",
            "--home",
            "/tmp/warden",
            "--python",
            "python3",
        ])
        .unwrap();
        assert!(matches!(
            arguments.command,
            Command::Start {
                home: Some(_),
                python: Some(_),
                manage_gui: false,
            }
        ));
    }

    #[test]
    fn existing_health_command_remains_compatible() {
        let arguments = Arguments::try_parse_from(["warden", "health"]).unwrap();
        assert!(matches!(arguments.command, Command::Health));
    }
}
