mod api_keys;
mod codex;
mod config;
mod openai;
mod server;
mod sse;
mod status;

use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "codex-relay", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the OpenAI-compatible HTTP server.
    Serve(config::ServeArgs),
    /// Sign in to a ChatGPT account with the device-code flow.
    Login(config::LoginArgs),
    /// Probe the local /healthz endpoint (used by the container HEALTHCHECK).
    Healthcheck {
        #[arg(
            long,
            env = "CODEX_RELAY_HEALTHCHECK_URL",
            default_value = "http://127.0.0.1:8080/healthz"
        )]
        url: String,
    },
    /// Print sign-in state and subscription usage limits of a running relay.
    Status {
        #[arg(
            long,
            env = "CODEX_RELAY_HEALTHCHECK_URL",
            default_value = "http://127.0.0.1:8080/healthz"
        )]
        url: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let result = match Cli::parse().command {
        Command::Serve(args) => server::run(args).await,
        Command::Login(args) => codex::login::run(&args.data_dir).await,
        Command::Healthcheck { url } => healthcheck(&url).await,
        Command::Status { url } => status::run(&url).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn healthcheck(url: &str) -> anyhow::Result<()> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
