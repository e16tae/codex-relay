//! Client API keys that services (Nextcloud, n8n, ...) use to call the relay.

use std::path::Path;

use anyhow::{Context, bail};
use subtle::ConstantTimeEq;

const MIN_SECRET_LEN: usize = 16;

struct ApiKey {
    name: String,
    secret: String,
}

pub struct ApiKeys(Vec<ApiKey>);

impl ApiKeys {
    /// Loads keys from an inline `name:key,name:key` list and/or a file with one pair per line.
    pub fn load(inline: Option<&str>, file: Option<&Path>) -> anyhow::Result<Self> {
        let mut keys = Vec::new();
        if let Some(inline) = inline {
            for entry in inline.split(',') {
                push_entry(&mut keys, entry)?;
            }
        }
        if let Some(path) = file {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading API keys file {}", path.display()))?;
            for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
                push_entry(&mut keys, line)?;
            }
        }
        if keys.is_empty() {
            bail!(
                "no client API keys configured; set CODEX_RELAY_API_KEYS or CODEX_RELAY_API_KEYS_FILE"
            );
        }
        Ok(Self(keys))
    }

    /// Returns the client name for a presented key. Compares every key in constant time.
    pub fn verify(&self, presented: &str) -> Option<&str> {
        let mut found = None;
        for key in &self.0 {
            if bool::from(key.secret.as_bytes().ct_eq(presented.as_bytes())) {
                found = Some(key.name.as_str());
            }
        }
        found
    }
}

fn push_entry(keys: &mut Vec<ApiKey>, raw: &str) -> anyhow::Result<()> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(());
    }
    let (name, secret) = raw
        .split_once(':')
        .context("API key entries must look like `name:key`")?;
    let (name, secret) = (name.trim(), secret.trim());
    if name.is_empty() {
        bail!("API key entry has an empty name");
    }
    if secret.len() < MIN_SECRET_LEN {
        bail!("API key for `{name}` is shorter than {MIN_SECRET_LEN} characters");
    }
    if keys.iter().any(|k| k.name == name) {
        bail!("duplicate API key name `{name}`");
    }
    keys.push(ApiKey {
        name: name.to_owned(),
        secret: secret.to_owned(),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_named_keys() {
        let keys = ApiKeys::load(
            Some("nextcloud:aaaaaaaaaaaaaaaa, n8n:bbbbbbbbbbbbbbbb"),
            None,
        )
        .unwrap();
        assert_eq!(keys.verify("aaaaaaaaaaaaaaaa"), Some("nextcloud"));
        assert_eq!(keys.verify("bbbbbbbbbbbbbbbb"), Some("n8n"));
        assert_eq!(keys.verify("cccccccccccccccc"), None);
        assert_eq!(keys.verify(""), None);
    }

    #[test]
    fn rejects_bad_entries() {
        assert!(ApiKeys::load(None, None).is_err());
        assert!(ApiKeys::load(Some("nokey"), None).is_err());
        assert!(ApiKeys::load(Some("short:abc"), None).is_err());
        assert!(ApiKeys::load(Some("a:aaaaaaaaaaaaaaaa,a:bbbbbbbbbbbbbbbb"), None).is_err());
    }
}
