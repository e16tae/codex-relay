//! Talking to OpenAI's ChatGPT/Codex sign-in and backend.
//!
//! The endpoints and client ID mirror the open-source Codex CLI (`codex-rs/login`).

pub mod backend;
pub mod credentials;
pub mod login;
pub mod quota;
pub mod tokens;

use std::time::Duration;

/// OAuth issuer used by the Codex CLI.
pub const ISSUER: &str = "https://auth.openai.com";
/// Public OAuth client ID of the Codex CLI.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub fn http_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("codex-relay/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()
}
