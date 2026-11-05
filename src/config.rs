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
}

#[derive(Args)]
pub struct LoginArgs {
    /// Directory where the ChatGPT credentials are stored.
    #[arg(long, env = "CODEX_RELAY_DATA_DIR", default_value = "/data")]
    pub data_dir: PathBuf,
}
