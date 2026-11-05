//! `codex-relay status`: a readable summary of `/healthz` (the image has no curl).

use std::time::Duration;

use serde_json::Value;

use crate::codex::quota::human_duration;

pub async fn run(url: &str) -> anyhow::Result<()> {
    // `/healthz` answers 503 with details when a sign-in is needed, so read the body either way.
    let body: Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?
        .get(url)
        .send()
        .await?
        .json()
        .await?;
    println!("{}", render(&body));
    Ok(())
}

fn render(h: &Value) -> String {
    let s = |v: &Value| v.as_str().unwrap_or("?").to_owned();
    let secs = |v: &Value| v.as_i64().map(human_duration);
    let mut lines = vec![format!(
        "codex-relay {} — {}",
        s(&h["version"]),
        s(&h["status"])
    )];

    let c = &h["chatgpt"];
    let mut sign_in = s(&c["state"]);
    if let Some(plan) = c["plan"].as_str() {
        sign_in += &format!(" (plan {plan})");
    }
    if let Some(ago) = secs(&c["last_refresh_secs_ago"]) {
        sign_in += &format!(", token refreshed {ago} ago");
    }
    lines.push(format!("ChatGPT sign-in : {sign_in}"));
    for key in ["reason", "action", "last_error"] {
        if let Some(text) = c[key].as_str() {
            lines.push(format!("                  {key}: {text}"));
        }
    }

    let q = &h["usage_limits"];
    if q["observed"].as_bool() != Some(true) {
        lines.push("Usage limits    : unknown until the first request".into());
        return lines.join("\n");
    }
    for key in ["primary", "secondary"] {
        let w = &q[key];
        let Some(used) = w["used_percent"].as_f64() else {
            continue;
        };
        let label = match w["window_minutes"].as_u64() {
            Some(m) => format!("{} window", window_length(m)),
            None => format!("{key} window"),
        };
        let mut line = format!("{label:<16}: {used:.0}% used");
        if let Some(resets) = w["resets_in"].as_str() {
            line += &format!(", resets in {resets}");
        }
        lines.push(line);
    }
    if let Some(credits) = q["credits"].as_object() {
        let text = if credits.get("unlimited").and_then(Value::as_bool) == Some(true) {
            "unlimited".to_owned()
        } else {
            credits
                .get("balance")
                .and_then(Value::as_f64)
                .map_or_else(|| "none".to_owned(), |b| format!("{b:.0}"))
        };
        lines.push(format!("Credits         : {text}"));
    }
    if let Some(ago) = secs(&q["observed_secs_ago"]) {
        lines.push(format!(
            "Observed        : {ago} ago (updates with each request)"
        ));
    }
    lines.join("\n")
}

/// 10080 → "7-day", 300 → "5-hour".
fn window_length(minutes: u64) -> String {
    if minutes % 1440 == 0 {
        format!("{}-day", minutes / 1440)
    } else if minutes % 60 == 0 {
        format!("{}-hour", minutes / 60)
    } else {
        format!("{minutes}-minute")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_ready_status() {
        let text = render(&json!({
            "status": "ok", "version": "0.1.0",
            "chatgpt": { "state": "ready", "plan": "pro", "last_refresh_secs_ago": 7200 },
            "usage_limits": {
                "observed": true, "observed_secs_ago": 180,
                "primary": { "used_percent": 35.0, "window_minutes": 10080, "resets_in": "5d 21h" },
                "credits": { "has_credits": true, "unlimited": false, "balance": 62500.0 }
            }
        }));
        assert!(text.contains("codex-relay 0.1.0 — ok"));
        assert!(text.contains("ready (plan pro), token refreshed 2h 0m ago"));
        assert!(text.contains("7-day window    : 35% used, resets in 5d 21h"));
        assert!(text.contains("Credits         : 62500"));
        assert!(text.contains("Observed        : 3m ago"));
    }

    #[test]
    fn renders_signed_out_status() {
        let text = render(&json!({
            "status": "unavailable", "version": "0.1.0",
            "chatgpt": { "state": "signed_out", "action": "run `codex-relay login`" },
            "usage_limits": { "observed": false }
        }));
        assert!(text.contains("— unavailable"));
        assert!(text.contains("action: run `codex-relay login`"));
        assert!(text.contains("unknown until the first request"));
    }
}
