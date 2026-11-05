//! Subscription usage limits, read from the `x-codex-*` headers the backend attaches
//! to every response. No extra requests are made, so the data is as fresh as the last call.

use std::sync::RwLock;

use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::{Value, json};

use super::credentials::now_unix;

/// Usage levels (percent) that are logged once when first crossed.
const WARN_LEVELS: [f64; 3] = [80.0, 90.0, 95.0];

#[derive(Clone, Serialize, PartialEq, Debug)]
pub struct Window {
    pub used_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

#[derive(Clone, Serialize, PartialEq, Debug)]
pub struct Credits {
    pub has_credits: bool,
    pub unlimited: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
}

#[derive(Clone, Serialize, PartialEq, Debug)]
pub struct Snapshot {
    pub observed_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_limit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<Window>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary: Option<Window>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<Credits>,
}

impl Snapshot {
    /// Parses the backend's rate-limit headers; `None` if the response carried none.
    pub fn from_headers(headers: &HeaderMap, now: i64) -> Option<Self> {
        let get = |name: &str| {
            headers
                .get(format!("x-codex-{name}"))
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let window = |prefix: &str| {
            let used_percent = get(&format!("{prefix}-used-percent"))?
                .parse::<f64>()
                .ok()?;
            let window_minutes = get(&format!("{prefix}-window-minutes"))
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|m| *m > 0);
            let resets_at = get(&format!("{prefix}-reset-at"))
                .and_then(|v| v.parse::<i64>().ok())
                .or_else(|| {
                    get(&format!("{prefix}-reset-after-seconds"))
                        .and_then(|v| v.parse::<i64>().ok())
                        .map(|secs| now + secs)
                })
                .filter(|at| *at > 0);
            // A window with no length and nothing used is the backend's "not in use".
            if window_minutes.is_none() && used_percent == 0.0 {
                return None;
            }
            Some(Window {
                used_percent,
                window_minutes,
                resets_at,
            })
        };
        let primary = window("primary");
        let secondary = window("secondary");
        let credits = get("credits-has-credits").map(|has| Credits {
            has_credits: has.eq_ignore_ascii_case("true"),
            unlimited: get("credits-unlimited").is_some_and(|v| v.eq_ignore_ascii_case("true")),
            balance: get("credits-balance").and_then(|v| v.parse().ok()),
        });
        if primary.is_none() && secondary.is_none() && credits.is_none() {
            return None;
        }
        Some(Self {
            observed_at: now,
            plan: get("plan-type").map(str::to_owned),
            active_limit: get("active-limit").map(str::to_owned),
            primary,
            secondary,
            credits,
        })
    }

    fn windows(&self) -> impl Iterator<Item = (&'static str, &Window)> {
        [("primary", &self.primary), ("secondary", &self.secondary)]
            .into_iter()
            .filter_map(|(name, w)| w.as_ref().map(|w| (name, w)))
    }

    /// The most used window.
    fn highest(&self) -> Option<(&'static str, &Window)> {
        self.windows()
            .max_by(|a, b| a.1.used_percent.total_cmp(&b.1.used_percent))
    }
}

#[derive(Default)]
pub struct QuotaTracker {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    snapshot: Option<Snapshot>,
    /// Highest warning level already logged for the current window.
    warned: f64,
}

impl QuotaTracker {
    pub fn observe(&self, headers: &HeaderMap) {
        let now = now_unix();
        let Some(snapshot) = Snapshot::from_headers(headers, now) else {
            return;
        };
        let mut inner = self.inner.write().unwrap();
        if let Some((name, window)) = snapshot.highest() {
            let used = window.used_percent;
            if used < inner.warned {
                // The window rolled over.
                inner.warned = 0.0;
            }
            if let Some(level) = WARN_LEVELS
                .iter()
                .copied()
                .filter(|l| used >= *l && *l > inner.warned)
                .next_back()
            {
                tracing::warn!(
                    window = name,
                    used_percent = used,
                    resets_in = %window.resets_at.map(|at| human_duration(at - now)).unwrap_or_default(),
                    "ChatGPT usage limit is at {level}% or more"
                );
                inner.warned = level;
            }
        }
        inner.snapshot = Some(snapshot);
    }

    /// Details for `/healthz`.
    pub fn status(&self) -> Value {
        let inner = self.inner.read().unwrap();
        let Some(snapshot) = &inner.snapshot else {
            return json!({ "observed": false, "note": "filled in after the first request" });
        };
        let now = now_unix();
        let mut status = serde_json::to_value(snapshot).unwrap_or_default();
        status["observed"] = true.into();
        status["observed_secs_ago"] = (now - snapshot.observed_at).into();
        for (name, window) in snapshot.windows() {
            if let Some(at) = window.resets_at {
                status[name]["resets_in"] = human_duration(at - now).into();
            }
        }
        status
    }
}

/// "When does it reset?" text for usage-limit errors, from the error response's headers.
pub fn reset_hint(headers: &HeaderMap) -> Option<String> {
    let now = now_unix();
    let snapshot = Snapshot::from_headers(headers, now)?;
    let (_, window) = snapshot.highest()?;
    let at = window.resets_at?;
    Some(format!(
        "ChatGPT usage limit {:.0}% used; resets in {}",
        window.used_percent,
        human_duration(at - now)
    ))
}

pub fn human_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    // The header set the backend actually sent for a Pro account.
    fn real(primary_used: &str) -> HeaderMap {
        headers(&[
            ("x-codex-active-limit", "premium"),
            ("x-codex-plan-type", "pro"),
            ("x-codex-primary-used-percent", primary_used),
            ("x-codex-secondary-used-percent", "0"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-secondary-window-minutes", "0"),
            ("x-codex-primary-reset-after-seconds", "508191"),
            ("x-codex-secondary-reset-after-seconds", "0"),
            ("x-codex-primary-reset-at", "1794353020"),
            ("x-codex-secondary-reset-at", ""),
            ("x-codex-credits-has-credits", "True"),
            ("x-codex-credits-balance", "62500.0000000000"),
            ("x-codex-credits-unlimited", "False"),
        ])
    }

    #[test]
    fn parses_real_headers() {
        let s = Snapshot::from_headers(&real("35"), 1_000).unwrap();
        assert_eq!(s.plan.as_deref(), Some("pro"));
        assert_eq!(
            s.primary,
            Some(Window {
                used_percent: 35.0,
                window_minutes: Some(10080),
                resets_at: Some(1_794_353_020)
            })
        );
        assert_eq!(s.secondary, None, "unused secondary window is dropped");
        let credits = s.credits.unwrap();
        assert!(credits.has_credits && !credits.unlimited);
        assert_eq!(credits.balance, Some(62_500.0));
    }

    #[test]
    fn ignores_responses_without_limits() {
        assert!(Snapshot::from_headers(&HeaderMap::new(), 0).is_none());
    }

    #[test]
    fn falls_back_to_reset_after_seconds() {
        let h = headers(&[
            ("x-codex-primary-used-percent", "50"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-after-seconds", "120"),
        ]);
        let s = Snapshot::from_headers(&h, 1_000).unwrap();
        assert_eq!(s.primary.unwrap().resets_at, Some(1_120));
    }

    #[test]
    fn tracks_warning_levels_and_rollover() {
        let tracker = QuotaTracker::default();
        tracker.observe(&real("35"));
        assert_eq!(tracker.inner.read().unwrap().warned, 0.0);
        tracker.observe(&real("91"));
        assert_eq!(tracker.inner.read().unwrap().warned, 90.0);
        tracker.observe(&real("92"));
        assert_eq!(tracker.inner.read().unwrap().warned, 90.0);
        tracker.observe(&real("3"));
        assert_eq!(tracker.inner.read().unwrap().warned, 0.0);
        let status = tracker.status();
        assert_eq!(status["primary"]["used_percent"], 3.0);
        assert!(status["primary"]["resets_in"].is_string());
    }

    #[test]
    fn formats_durations() {
        assert_eq!(human_duration(59), "0m");
        assert_eq!(human_duration(3_660), "1h 1m");
        assert_eq!(human_duration(508_191), "5d 21h");
        assert_eq!(human_duration(-5), "0m");
    }
}
