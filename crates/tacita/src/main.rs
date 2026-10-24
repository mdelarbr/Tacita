use std::path::Path;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tacita::config::{Config, ConfigError, Protection, Provider, State};
use tacita::daemon::{DaemonOptions, Server};
use tacita::paths;

#[derive(Parser)]
#[command(name = "tacita", about = "Local privacy proxy for Claude Code")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the config file if it does not exist
    Init,
    /// Show the active provider, protection, base URL and daemon state
    Status {
        /// Print only the base URL
        #[arg(long)]
        base_url: bool,
    },
    /// Choose the provider for new requests
    Use {
        #[command(subcommand)]
        provider: UseCmd,
    },
    /// Store a provider key in the macOS Keychain
    Key {
        #[command(subcommand)]
        action: KeyCmd,
    },
    /// Run the proxy in the foreground on 127.0.0.1 (Ctrl-C stops it)
    Daemon,
    /// Claude Code hooks
    Hook {
        #[command(subcommand)]
        event: HookCmd,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    /// Deny tool calls that still hold a Tacita stand-in (reads JSON on stdin)
    PreToolUse,
}

#[derive(Subcommand)]
enum UseCmd {
    Anthropic,
    Openrouter {
        #[arg(long)]
        model: String,
    },
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Prompt for the OpenRouter key (input hidden) and store it
    Set {
        #[arg(value_parser = ["openrouter"])]
        name: String,
    },
}

/// `Ok` with the trimmed key when it looks like an OpenRouter key. The
/// error never carries the input.
fn check_openrouter_key(input: &str) -> Result<&str, String> {
    let key = input.trim();
    if key.is_empty() {
        return Err("no key entered".into());
    }
    if !key.starts_with("sk-or-") || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("that does not look like an OpenRouter key (sk-or-…)".into());
    }
    Ok(key)
}

#[cfg(target_os = "macos")]
fn key_set(name: &str) -> Result<(), String> {
    use tacita::secrets::{KeychainStore, SecretStore};
    let input = rpassword::prompt_password("OpenRouter key (input hidden): ")
        .map_err(|e| format!("cannot read the key: {e}"))?;
    let key = check_openrouter_key(&input)?;
    KeychainStore::new("dev.tacita")
        .set(name, key)
        .map_err(|e| format!("cannot store the key in the Keychain: {e}"))?;
    println!("Stored in the macOS Keychain (service dev.tacita, account {name}).");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn key_set(_name: &str) -> Result<(), String> {
    Err("the Keychain is only available on macOS".into())
}

fn parse_model(s: &str) -> Result<String, String> {
    if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("must be non-empty with no whitespace or control characters".into());
    }
    Ok(s.to_string())
}

fn load_config(home: &Path) -> Result<Config, String> {
    match Config::load(home) {
        Err(ConfigError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Err("not initialised: run `tacita init`".into())
        }
        other => other.map_err(|e| e.to_string()),
    }
}

fn session_line(cfg: &Config) -> String {
    format!(
        "For one Claude Code session:  ANTHROPIC_BASE_URL={} claude",
        cfg.base_url()
    )
}

async fn daemon_running(base_url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(1))
        .build()
    else {
        return false;
    };
    match client.head(format!("{base_url}/api/hello")).send().await {
        Ok(r) => r.status() == reqwest::StatusCode::OK,
        Err(_) => false,
    }
}

async fn print_status(home: &Path, cfg: &Config) -> Result<(), String> {
    let state = State::load(home).map_err(|e| e.to_string())?;
    let (provider, protection) = match state.provider {
        Provider::Anthropic => ("anthropic".to_string(), cfg.anthropic.protection),
        Provider::Openrouter => (
            format!(
                "openrouter (model {})",
                state.openrouter_model.as_deref().unwrap_or("?")
            ),
            cfg.openrouter.protection,
        ),
    };
    let protection = match protection {
        Protection::Off => "off",
        Protection::Pairs => "pairs",
    };
    let base_url = cfg.base_url();
    let daemon = if daemon_running(&base_url).await {
        "running"
    } else {
        "not running"
    };
    println!("provider:   {provider}");
    println!("protection: {protection}");
    println!("base URL:   {base_url}");
    println!("daemon:     {daemon}");
    Ok(())
}

/// Binds 127.0.0.1 only and serves until Ctrl-C.
async fn run_daemon(home: std::path::PathBuf) -> Result<(), String> {
    use tokio::signal::unix::{SignalKind, signal};
    let cfg = load_config(&home)?;
    let port = cfg.port;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                format!("port {port} is in use (is another tacita daemon running?)")
            } else {
                format!("cannot listen on 127.0.0.1:{port}: {e}")
            }
        })?;
    // Registered before the banner, so a Ctrl-C right after it is caught.
    let mut interrupt =
        signal(SignalKind::interrupt()).map_err(|e| format!("cannot catch Ctrl-C: {e}"))?;
    // The banner only once the config is reloaded and the log is open.
    let server =
        Server::new(DaemonOptions::production(home), listener).map_err(|e| format!("{e:#}"))?;
    println!("Tacita listening on {}", cfg.base_url());
    tokio::select! {
        served = server.run() => served.map_err(|e| format!("{e:#}")),
        _ = interrupt.recv() => {
            println!("Tacita stopped");
            Ok(())
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let home = paths::home();
    match cli.command {
        Cmd::Init => {
            let file = home.join("config.toml");
            let (cfg, line) = if file.exists() {
                (
                    load_config(&home)?,
                    format!("Already initialised: {}", file.display()),
                )
            } else {
                let cfg = Config::create_default(&home).map_err(|e| e.to_string())?;
                (cfg, format!("Created {}", file.display()))
            };
            println!("{line}");
            println!("{}", session_line(&cfg));
        }
        Cmd::Status { base_url } => {
            let cfg = load_config(&home)?;
            if base_url {
                println!("{}", cfg.base_url());
            } else {
                print_status(&home, &cfg).await?;
            }
        }
        Cmd::Use { provider } => {
            if let UseCmd::Openrouter { model } = &provider {
                parse_model(model)?;
            }
            let cfg = load_config(&home)?;
            let state = match provider {
                UseCmd::Anthropic => State {
                    provider: Provider::Anthropic,
                    openrouter_model: None,
                },
                UseCmd::Openrouter { model } => State {
                    provider: Provider::Openrouter,
                    openrouter_model: Some(model),
                },
            };
            state.save(&home).map_err(|e| e.to_string())?;
            print_status(&home, &cfg).await?;
        }
        Cmd::Key {
            action: KeyCmd::Set { name },
        } => key_set(&name)?,
        Cmd::Daemon => run_daemon(home).await?,
        Cmd::Hook { .. } => unreachable!("handled in main"),
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    // The hook never goes through `run`: its exit code is the contract.
    if let Cmd::Hook {
        event: HookCmd::PreToolUse,
    } = cli.command
    {
        std::process::exit(tacita::hook::run());
    }
    if let Err(msg) = run(cli).await {
        eprintln!("tacita: {msg}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::check_openrouter_key;

    #[test]
    fn key_check_trims_and_refuses_without_echo() {
        assert_eq!(check_openrouter_key("  sk-or-v1-abc\n"), Ok("sk-or-v1-abc"));
        for bad in [
            "",
            "   ",
            "sk-ant-oat01-SECRET",
            "sk-or-a b",
            "sk-or-\u{e9}x",
            "sk-or-a\tb",
        ] {
            let e = check_openrouter_key(bad).unwrap_err();
            assert!(!e.contains("SECRET") && !e.contains("a b"), "{e}");
        }
        assert_eq!(check_openrouter_key("").unwrap_err(), "no key entered");
    }
}
