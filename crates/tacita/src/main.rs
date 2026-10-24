use std::path::Path;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tacita::config::{Config, ConfigError, Protection, Provider, State};
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
}

#[derive(Subcommand)]
enum UseCmd {
    Anthropic,
    Openrouter {
        #[arg(long)]
        model: String,
    },
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
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(msg) = run(cli).await {
        eprintln!("tacita: {msg}");
        std::process::exit(1);
    }
}
