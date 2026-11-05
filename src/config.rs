use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Args;

#[derive(Args)]
pub struct ServeArgs {
    /// Address to listen on.
    #[arg(long, env = "CODEX_RELAY_LISTEN", default_value = "0.0.0.0:8080")]
    pub listen: SocketAddr,

    /// Directory holding ChatGPT credentials and runtime state.
    #[arg(long, env = "CODEX_RELAY_DATA_DIR", default_value = "/data")]
    pub data_dir: PathBuf,

    /// Client API keys as comma-separated `name:key` pairs.
    #[arg(long, env = "CODEX_RELAY_API_KEYS", hide_env_values = true)]
    pub api_keys: Option<String>,

    /// File with one `name:key` pair per line (Docker secrets friendly).
    #[arg(long, env = "CODEX_RELAY_API_KEYS_FILE")]
    pub api_keys_file: Option<PathBuf>,

    /// Base URL of the ChatGPT Codex backend.
    #[arg(
        long,
        env = "CODEX_RELAY_UPSTREAM_URL",
        default_value = "https://chatgpt.com/backend-api/codex"
    )]
    pub upstream_url: String,

    /// Codex CLI version reported to the backend (affects the model list).
    #[arg(long, env = "CODEX_RELAY_CLIENT_VERSION", default_value = "0.166.0")]
    pub client_version: String,

    /// Reasoning effort used when a request does not set `reasoning_effort`.
    #[arg(long, env = "CODEX_RELAY_DEFAULT_REASONING_EFFORT")]
    pub default_reasoning_effort: Option<String>,
}

#[derive(Args)]
pub struct LoginArgs {
    /// Directory where the ChatGPT credentials are stored.
    #[arg(long, env = "CODEX_RELAY_DATA_DIR", default_value = "/data")]
    pub data_dir: PathBuf,
}
