//! Keeps the ChatGPT access token fresh and picks up new sign-ins from disk.
//!
//! - Refreshes shortly before the access token expires.
//! - Refreshes at least once a day even when idle, so the refresh token never goes stale.
//! - Only one refresh runs at a time; refresh tokens are single-use.
//! - A rejected refresh token stops the relay until `codex-relay login` is run again.
//!   There is no fallback.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::credentials::{Credentials, now_unix};
use super::{CLIENT_ID, ISSUER, http_client};

/// Refresh when the access token has less than this many seconds left.
const EXPIRY_MARGIN: i64 = 5 * 60;
/// Refresh at least this often, even when idle.
const KEEPALIVE: i64 = 24 * 60 * 60;
/// Background back-off after a temporary refresh failure.
const RETRY_AFTER: i64 = 5 * 60;
/// How often the background task checks the credentials file and the keepalive.
const TICK: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq)]
enum State {
    SignedOut,
    Ready,
    /// The refresh token was rejected or the file is unreadable; needs a new `login`.
    Dead(String),
}

struct Inner {
    creds: Option<Credentials>,
    state: State,
    file_mtime: Option<SystemTime>,
    last_error: Option<String>,
    retry_at: i64,
}

pub struct TokenManager {
    path: PathBuf,
    http: reqwest::Client,
    inner: RwLock<Inner>,
    refresh_lock: Mutex<()>,
}

/// What an upstream request needs.
#[allow(dead_code)] // used by the upstream client in the next step
pub struct Access {
    pub access_token: String,
    pub account_id: String,
}

#[derive(Debug)]
pub enum TokenError {
    SignedOut,
    Dead(String),
    Unavailable(String),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SignedOut => write!(f, "not signed in to ChatGPT; run `codex-relay login`"),
            Self::Dead(reason) => write!(
                f,
                "ChatGPT sign-in is no longer valid ({reason}); run `codex-relay login`"
            ),
            Self::Unavailable(msg) => write!(f, "could not refresh the ChatGPT token: {msg}"),
        }
    }
}

impl std::error::Error for TokenError {}

#[derive(Clone, Copy)]
enum Due {
    /// The access token is about to expire.
    Expiring,
    /// The access token is about to expire, or the daily keepalive is due.
    Keepalive,
}

enum RefreshFailure {
    Permanent(String),
    Transient(String),
}

#[derive(Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

impl TokenManager {
    pub fn new(data_dir: &Path) -> anyhow::Result<Self> {
        let manager = Self {
            path: Credentials::path(data_dir),
            http: http_client(Duration::from_secs(45))?,
            inner: RwLock::new(Inner {
                creds: None,
                state: State::SignedOut,
                file_mtime: None,
                last_error: None,
                retry_at: 0,
            }),
            refresh_lock: Mutex::new(()),
        };
        manager.load_from_disk(file_mtime(&manager.path));
        Ok(manager)
    }

    /// Starts the background task for file reloads and the keepalive.
    pub fn spawn_maintenance(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                this.reload_if_changed();
                let backing_off = this.inner.read().unwrap().retry_at > now_unix();
                if !backing_off && matches!(this.due(Due::Keepalive), Ok(true)) {
                    // Failures are recorded in the state and logged.
                    let _ = this.refresh(Due::Keepalive).await;
                }
            }
        });
    }

    /// Returns a usable access token, refreshing it first if it is about to expire.
    #[allow(dead_code)] // used by the upstream client in the next step
    pub async fn access(&self) -> Result<Access, TokenError> {
        self.reload_if_changed();
        if self.due(Due::Expiring)? {
            if let Err(err) = self.refresh(Due::Expiring).await {
                // A temporary failure is fine while the current token still works.
                if !matches!(err, TokenError::Unavailable(_)) || !self.still_valid() {
                    return Err(err);
                }
            }
        }
        let inner = self.inner.read().unwrap();
        match (&inner.state, &inner.creds) {
            (State::Ready, Some(creds)) => Ok(Access {
                access_token: creds.access_token.clone(),
                account_id: creds.account_id.clone(),
            }),
            (State::Dead(reason), _) => Err(TokenError::Dead(reason.clone())),
            _ => Err(TokenError::SignedOut),
        }
    }

    /// `(healthy, details)` for `/healthz`. Never includes tokens or the account email.
    pub fn status(&self) -> (bool, Value) {
        let inner = self.inner.read().unwrap();
        let now = now_unix();
        let mut status = match &inner.state {
            State::Ready => json!({ "state": "ready" }),
            State::SignedOut => {
                json!({ "state": "signed_out", "action": "run `codex-relay login`" })
            }
            State::Dead(reason) => json!({
                "state": "sign_in_required",
                "reason": reason,
                "action": "run `codex-relay login`"
            }),
        };
        if let Some(creds) = &inner.creds {
            status["plan"] = creds.plan.clone().into();
            status["access_expires_in_secs"] = creds.access_expires_at().map(|e| e - now).into();
            status["last_refresh_secs_ago"] = (now - creds.last_refresh).into();
        }
        if let Some(err) = &inner.last_error {
            status["last_error"] = err.clone().into();
        }
        (inner.state == State::Ready, status)
    }

    /// Reloads the credentials file if it changed on disk, e.g. after `login`.
    pub fn reload_if_changed(&self) {
        let mtime = file_mtime(&self.path);
        if self.inner.read().unwrap().file_mtime != mtime {
            self.load_from_disk(mtime);
        }
    }

    fn load_from_disk(&self, mtime: Option<SystemTime>) {
        let mut inner = self.inner.write().unwrap();
        inner.file_mtime = mtime;
        inner.last_error = None;
        inner.retry_at = 0;
        match Credentials::load(&self.path) {
            Ok(Some(creds)) => {
                tracing::info!(
                    email = creds.email.as_deref().unwrap_or("unknown"),
                    plan = creds.plan.as_deref().unwrap_or("unknown"),
                    "loaded ChatGPT credentials"
                );
                inner.creds = Some(creds);
                inner.state = State::Ready;
            }
            Ok(None) => {
                tracing::warn!("not signed in to ChatGPT; run `codex-relay login`");
                inner.creds = None;
                inner.state = State::SignedOut;
            }
            Err(err) => {
                tracing::error!("{err:#}");
                inner.creds = None;
                inner.state = State::Dead(format!("cannot read credentials: {err:#}"));
            }
        }
    }

    fn due(&self, why: Due) -> Result<bool, TokenError> {
        let inner = self.inner.read().unwrap();
        let creds = match (&inner.state, &inner.creds) {
            (State::Ready, Some(creds)) => creds,
            (State::Dead(reason), _) => return Err(TokenError::Dead(reason.clone())),
            _ => return Err(TokenError::SignedOut),
        };
        let now = now_unix();
        let expiring = creds
            .access_expires_at()
            .is_none_or(|exp| exp - now < EXPIRY_MARGIN);
        Ok(match why {
            Due::Expiring => expiring,
            Due::Keepalive => expiring || now - creds.last_refresh >= KEEPALIVE,
        })
    }

    fn still_valid(&self) -> bool {
        let inner = self.inner.read().unwrap();
        inner
            .creds
            .as_ref()
            .and_then(Credentials::access_expires_at)
            .is_some_and(|exp| exp > now_unix() + 30)
    }

    async fn refresh(&self, why: Due) -> Result<(), TokenError> {
        let _guard = self.refresh_lock.lock().await;
        self.reload_if_changed();
        // Another caller may have refreshed while this one waited for the lock.
        if !self.due(why)? {
            return Ok(());
        }
        let (refresh_token, mtime) = {
            let inner = self.inner.read().unwrap();
            let token = inner.creds.as_ref().map(|c| c.refresh_token.clone());
            (token.unwrap_or_default(), inner.file_mtime)
        };

        let result = self.request_refresh(&refresh_token).await;

        // A new `login` replaced the file meanwhile: keep that sign-in, drop this result.
        if file_mtime(&self.path) != mtime {
            self.reload_if_changed();
            return Ok(());
        }

        let mut inner = self.inner.write().unwrap();
        match result {
            Ok(response) => {
                let current = inner.creds.as_ref().expect("ready state has credentials");
                let next = match current.refreshed(
                    response.id_token,
                    response.access_token.unwrap_or_default(),
                    response.refresh_token,
                ) {
                    Ok(next) => next,
                    Err(err) => {
                        let reason = format!("{err:#}");
                        tracing::error!(%reason, "unusable refresh response");
                        inner.state = State::Dead(reason.clone());
                        return Err(TokenError::Dead(reason));
                    }
                };
                // Keep the new tokens in memory even if saving fails: the old refresh
                // token has already been used up.
                if let Err(err) = next.save(&self.path) {
                    tracing::error!(
                        "refreshed tokens could not be saved and will be lost on restart: {err:#}"
                    );
                    inner.last_error = Some(format!("saving credentials failed: {err:#}"));
                } else {
                    inner.last_error = None;
                }
                inner.creds = Some(next);
                inner.file_mtime = file_mtime(&self.path);
                inner.retry_at = 0;
                tracing::info!("refreshed ChatGPT access token");
                Ok(())
            }
            Err(RefreshFailure::Permanent(reason)) => {
                tracing::error!(%reason, "ChatGPT sign-in is no longer valid; run `codex-relay login`");
                inner.state = State::Dead(reason.clone());
                Err(TokenError::Dead(reason))
            }
            Err(RefreshFailure::Transient(msg)) => {
                tracing::warn!(error = %msg, "token refresh failed; will retry");
                inner.last_error = Some(msg.clone());
                inner.retry_at = now_unix() + RETRY_AFTER;
                Err(TokenError::Unavailable(msg))
            }
        }
    }

    async fn request_refresh(
        &self,
        refresh_token: &str,
    ) -> Result<RefreshResponse, RefreshFailure> {
        let response = self
            .http
            .post(format!("{ISSUER}/oauth/token"))
            .json(&json!({
                "grant_type": "refresh_token",
                "client_id": CLIENT_ID,
                "refresh_token": refresh_token,
            }))
            .send()
            .await
            .map_err(|err| RefreshFailure::Transient(format!("request failed: {err}")))?;
        let status = response.status();
        if status.is_success() {
            let body: RefreshResponse = response.json().await.map_err(|err| {
                RefreshFailure::Transient(format!("invalid refresh response: {err}"))
            })?;
            if body.access_token.is_none() {
                return Err(RefreshFailure::Transient(
                    "refresh response has no access token".into(),
                ));
            }
            return Ok(body);
        }
        // The body may echo credentials, so keep only the error code.
        let code = response
            .json::<Value>()
            .await
            .ok()
            .and_then(|body| error_code(&body));
        Err(classify_failure(status, code.as_deref()))
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Extracts an OAuth error code from `{"error": "..."}` or `{"error": {"code": "..."}}`.
fn error_code(body: &Value) -> Option<String> {
    match body.get("error")? {
        Value::String(code) => Some(code.clone()),
        Value::Object(obj) => obj.get("code")?.as_str().map(str::to_owned),
        _ => None,
    }
}

fn classify_failure(status: StatusCode, code: Option<&str>) -> RefreshFailure {
    let code = code.map(str::to_ascii_lowercase);
    let known = match code.as_deref() {
        Some("refresh_token_expired") => Some("refresh token expired (unused for too long)"),
        Some("refresh_token_reused") => {
            Some("refresh token was already used (another client shares these credentials?)")
        }
        Some("refresh_token_invalidated") => {
            Some("refresh token revoked (password change, sign-out, or security action)")
        }
        _ => None,
    };
    if let Some(reason) = known {
        return RefreshFailure::Permanent(reason.into());
    }
    let invalid_grant =
        status == StatusCode::BAD_REQUEST && code.as_deref() == Some("invalid_grant");
    if status == StatusCode::UNAUTHORIZED || invalid_grant {
        let code = code.as_deref().unwrap_or("none");
        return RefreshFailure::Permanent(format!("refresh rejected (HTTP {status}, code {code})"));
    }
    RefreshFailure::Transient(format!("HTTP {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::credentials::tests::{fake_id_token, fake_jwt};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("codex-relay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn classifies_refresh_failures() {
        let permanent = |s, c| matches!(classify_failure(s, c), RefreshFailure::Permanent(_));
        assert!(permanent(
            StatusCode::BAD_REQUEST,
            Some("refresh_token_expired")
        ));
        assert!(permanent(
            StatusCode::BAD_REQUEST,
            Some("REFRESH_TOKEN_REUSED")
        ));
        assert!(permanent(
            StatusCode::BAD_REQUEST,
            Some("refresh_token_invalidated")
        ));
        assert!(permanent(StatusCode::BAD_REQUEST, Some("invalid_grant")));
        assert!(permanent(StatusCode::UNAUTHORIZED, None));
        assert!(!permanent(StatusCode::BAD_GATEWAY, None));
        assert!(!permanent(
            StatusCode::TOO_MANY_REQUESTS,
            Some("rate_limited")
        ));
    }

    #[test]
    fn extracts_error_codes() {
        assert_eq!(
            error_code(&json!({"error": "invalid_grant"})).as_deref(),
            Some("invalid_grant")
        );
        assert_eq!(
            error_code(&json!({"error": {"code": "refresh_token_expired"}})).as_deref(),
            Some("refresh_token_expired")
        );
        assert_eq!(error_code(&json!({"detail": "x"})), None);
    }

    #[test]
    fn picks_up_sign_in_and_schedules_keepalive() {
        let dir = temp_dir("tokens");
        let manager = TokenManager::new(&dir).unwrap();
        assert!(!manager.status().0);
        assert!(matches!(
            manager.due(Due::Keepalive),
            Err(TokenError::SignedOut)
        ));

        let access = fake_jwt(json!({ "exp": now_unix() + 3600 }));
        let mut creds =
            Credentials::from_tokens(fake_id_token("acct"), access, "rt".into()).unwrap();
        creds.save(&Credentials::path(&dir)).unwrap();
        manager.reload_if_changed();
        assert!(manager.status().0);
        assert!(!manager.due(Due::Expiring).unwrap());
        assert!(!manager.due(Due::Keepalive).unwrap());

        // A day-old refresh makes the keepalive due, but not the expiry check.
        creds.last_refresh -= KEEPALIVE;
        creds.save(&Credentials::path(&dir)).unwrap();
        // Make sure the mtime differs even on coarse filesystems.
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(Credentials::path(&dir))
            .unwrap()
            .set_modified(later)
            .unwrap();
        manager.reload_if_changed();
        assert!(!manager.due(Due::Expiring).unwrap());
        assert!(manager.due(Due::Keepalive).unwrap());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
