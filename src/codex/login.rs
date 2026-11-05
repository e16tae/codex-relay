//! Device-code sign-in, the same flow as `codex login --device-auth`.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Deserializer};
use serde_json::json;

use super::credentials::Credentials;
use super::{CLIENT_ID, ISSUER, http_client};

const CODE_LIFETIME: Duration = Duration::from_secs(15 * 60);

#[derive(Deserialize)]
struct UserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default, deserialize_with = "lenient_u64")]
    interval: u64,
}

#[derive(Deserialize)]
struct AuthorizationResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

pub async fn run(data_dir: &Path) -> anyhow::Result<()> {
    let http = http_client(Duration::from_secs(30))?;
    let accounts = format!("{ISSUER}/api/accounts");

    let response = http
        .post(format!("{accounts}/deviceauth/usercode"))
        .json(&json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .context("requesting a device code")?;
    if response.status() == StatusCode::NOT_FOUND {
        bail!("device code sign-in is not enabled for this account or server");
    }
    let code: UserCodeResponse = checked(response, "device code request")
        .await?
        .json()
        .await?;

    println!(
        "\nSign in to ChatGPT for codex-relay:\n\n  \
         1. Open   {ISSUER}/codex/device\n  \
         2. Enter  {}   (expires in 15 minutes)\n\n\
         Only continue if you started this sign-in yourself.\n",
        code.user_code
    );

    let authorization = poll_for_authorization(&http, &accounts, &code).await?;

    let response = http
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", authorization.authorization_code.as_str()),
            ("redirect_uri", &format!("{ISSUER}/deviceauth/callback")),
            ("code_verifier", authorization.code_verifier.as_str()),
        ])
        .send()
        .await
        .context("exchanging the authorization code")?;
    let tokens: TokenResponse = checked(response, "token exchange").await?.json().await?;

    let creds =
        Credentials::from_tokens(tokens.id_token, tokens.access_token, tokens.refresh_token)?;
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let path = Credentials::path(data_dir);
    creds.save(&path)?;

    println!(
        "Signed in as {} (plan: {}). Saved to {}.\n\
         A running relay picks this up automatically within 30 seconds.",
        creds.email.as_deref().unwrap_or("unknown"),
        creds.plan.as_deref().unwrap_or("unknown"),
        path.display()
    );
    Ok(())
}

async fn poll_for_authorization(
    http: &reqwest::Client,
    accounts: &str,
    code: &UserCodeResponse,
) -> anyhow::Result<AuthorizationResponse> {
    let interval = Duration::from_secs(if code.interval == 0 { 5 } else { code.interval });
    let started = Instant::now();
    loop {
        let response = http
            .post(format!("{accounts}/deviceauth/token"))
            .json(&json!({ "device_auth_id": code.device_auth_id, "user_code": code.user_code }))
            .send()
            .await
            .context("polling for approval")?;
        let status = response.status();
        if status.is_success() {
            return Ok(response.json().await?);
        }
        // 403/404 mean "not approved yet".
        if status != StatusCode::FORBIDDEN && status != StatusCode::NOT_FOUND {
            checked(response, "device authorization").await?;
            unreachable!("checked() fails on non-success statuses");
        }
        if started.elapsed() >= CODE_LIFETIME {
            bail!("the code expired before it was approved; run `login` again");
        }
        tokio::time::sleep(interval).await;
    }
}

/// Turns a non-success response into an error that includes the server's message.
async fn checked(response: reqwest::Response, what: &str) -> anyhow::Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let body: String = body.chars().take(500).collect();
    bail!("{what} failed with HTTP {status}: {body}");
}

/// Accepts the polling interval as either a number or a numeric string.
fn lenient_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("interval must be a positive integer")),
        serde_json::Value::String(s) => s.trim().parse().map_err(serde::de::Error::custom),
        serde_json::Value::Null => Ok(0),
        other => Err(serde::de::Error::custom(format!(
            "unexpected interval {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_interval_as_string_or_number() {
        for raw in [
            r#"{"device_auth_id":"d","user_code":"U","interval":"5"}"#,
            r#"{"device_auth_id":"d","usercode":"U","interval":5}"#,
        ] {
            let parsed: UserCodeResponse = serde_json::from_str(raw).unwrap();
            assert_eq!(parsed.interval, 5);
            assert_eq!(parsed.user_code, "U");
        }
        let parsed: UserCodeResponse =
            serde_json::from_str(r#"{"device_auth_id":"d","user_code":"U"}"#).unwrap();
        assert_eq!(parsed.interval, 0);
    }
}
