//! ChatGPT credentials persisted in `<data_dir>/auth.json`.
//!
//! Deliberately has no `Debug` impl so tokens never end up in logs.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};

const AUTH_CLAIM: &str = "https://api.openai.com/auth";

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    /// Unix seconds of the last successful sign-in or refresh.
    pub last_refresh: i64,
}

impl Credentials {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("auth.json")
    }

    /// Builds credentials from a fresh token set, reading account details from the ID token.
    pub fn from_tokens(
        id_token: String,
        access_token: String,
        refresh_token: String,
    ) -> anyhow::Result<Self> {
        let identity = Identity::from_id_token(&id_token)?;
        Ok(Self {
            id_token,
            access_token,
            refresh_token,
            account_id: identity.account_id,
            email: identity.email,
            plan: identity.plan,
            last_refresh: now_unix(),
        })
    }

    /// Returns a copy updated with the result of a refresh. Missing fields keep their old value.
    pub fn refreshed(
        &self,
        id_token: Option<String>,
        access_token: String,
        refresh_token: Option<String>,
    ) -> anyhow::Result<Self> {
        let mut next = self.clone();
        if let Some(id_token) = id_token {
            let identity = Identity::from_id_token(&id_token)?;
            if identity.account_id != self.account_id {
                bail!("refresh returned a token for a different ChatGPT account");
            }
            next.email = identity.email.or(next.email);
            next.plan = identity.plan.or(next.plan);
            next.id_token = id_token;
        }
        next.access_token = access_token;
        if let Some(refresh_token) = refresh_token {
            next.refresh_token = refresh_token;
        }
        next.last_refresh = now_unix();
        Ok(next)
    }

    /// Expiry of the access token (unix seconds), if it carries one.
    pub fn access_expires_at(&self) -> Option<i64> {
        jwt_claims(&self.access_token).ok()?.get("exp")?.as_i64()
    }

    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let creds =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(creds))
    }

    /// Writes the file atomically with owner-only permissions.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("json.tmp");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

struct Identity {
    account_id: String,
    email: Option<String>,
    plan: Option<String>,
}

impl Identity {
    fn from_id_token(id_token: &str) -> anyhow::Result<Self> {
        let claims = jwt_claims(id_token).context("decoding ID token")?;
        let auth = claims.get(AUTH_CLAIM);
        let field = |name: &str| {
            auth.and_then(|a| a.get(name))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        };
        Ok(Self {
            account_id: field("chatgpt_account_id").context("ID token has no ChatGPT account")?,
            plan: field("chatgpt_plan_type"),
            email: claims
                .get("email")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        })
    }
}

/// Decodes a JWT payload without verifying it; the issuer is trusted over TLS.
pub fn jwt_claims(token: &str) -> anyhow::Result<serde_json::Value> {
    let payload = token.split('.').nth(1).context("not a JWT")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("invalid JWT encoding")?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub fn fake_jwt(claims: serde_json::Value) -> String {
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            enc.encode(br#"{"alg":"none"}"#),
            enc.encode(claims.to_string())
        )
    }

    pub fn fake_id_token(account: &str) -> String {
        fake_jwt(json!({
            "email": "user@example.com",
            AUTH_CLAIM: { "chatgpt_account_id": account, "chatgpt_plan_type": "pro" }
        }))
    }

    #[test]
    fn reads_identity_and_expiry() {
        let access = fake_jwt(json!({ "exp": 1_900_000_000 }));
        let creds = Credentials::from_tokens(fake_id_token("acct-1"), access, "rt".into()).unwrap();
        assert_eq!(creds.account_id, "acct-1");
        assert_eq!(creds.plan.as_deref(), Some("pro"));
        assert_eq!(creds.email.as_deref(), Some("user@example.com"));
        assert_eq!(creds.access_expires_at(), Some(1_900_000_000));
    }

    #[test]
    fn refresh_keeps_missing_fields_and_rejects_other_accounts() {
        let creds =
            Credentials::from_tokens(fake_id_token("acct-1"), "a1".into(), "rt1".into()).unwrap();
        let next = creds.refreshed(None, "a2".into(), None).unwrap();
        assert_eq!(next.access_token, "a2");
        assert_eq!(next.refresh_token, "rt1");
        assert!(
            creds
                .refreshed(Some(fake_id_token("acct-2")), "a3".into(), None)
                .is_err()
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("codex-relay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = Credentials::path(&dir);
        assert!(Credentials::load(&path).unwrap().is_none());
        let creds =
            Credentials::from_tokens(fake_id_token("acct-1"), "a1".into(), "rt1".into()).unwrap();
        creds.save(&path).unwrap();
        let loaded = Credentials::load(&path).unwrap().unwrap();
        assert_eq!(loaded.refresh_token, "rt1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
