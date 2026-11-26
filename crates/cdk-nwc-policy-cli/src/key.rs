//! API keys: one NWC link + one Cashu pocket + its rules.
//!
//! A key is a pocket connection plus a small metadata record. Everything the
//! pocket can enforce lives in the policy ledger (expiry, methods, per-payment
//! cap) and the pocket registry (balance ceiling, refill cap). Rules the
//! pocket cannot see, such as which model a request uses, are stored here and
//! exported by `key list` for the gateway in front of the provider to enforce.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Rules of one key that are not already held by the policy ledger or the
/// pocket registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMeta {
    /// NWC connection ID (the key ID).
    pub key_id: String,
    /// Human-readable label.
    pub label: String,
    /// Models the key may use. Empty means any. Not visible in a bolt11, so
    /// the pocket cannot enforce this; the gateway must.
    #[serde(default)]
    pub allowed_models: Vec<String>,
    /// Largest invoice the pocket pays for this key, in sats.
    pub max_per_payment_sats: Option<u64>,
    /// Unix time the key was created.
    pub created_at: u64,
    /// SHA-256 (hex) of the key's API key for `key proxy`. The API key itself
    /// is shown once at creation and never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_sha256: Option<String>,
}

/// Default key metadata path, next to the policy ledger.
pub fn keys_path(policy_path: &Path) -> PathBuf {
    policy_path.with_extension("keys.json")
}

/// Read all key records (an absent file means none).
pub fn load_keys(path: &Path) -> Result<Vec<KeyMeta>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("parse key metadata"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).context("read key metadata"),
    }
}

/// Append one key record, writing atomically.
pub fn save_key(path: &Path, meta: KeyMeta) -> Result<()> {
    let mut keys = load_keys(path)?;
    keys.retain(|key| key.key_id != meta.key_id);
    keys.push(meta);
    write_private(path, &serde_json::to_vec_pretty(&keys)?)
}

/// Write `bytes` to `path` with `0600` permissions via a temp file + rename.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    crate::ensure_private_file(&temp)?;
    fs::write(&temp, bytes).context("write temp file")?;
    fs::rename(&temp, path).context("replace file")
}

/// Split a comma-separated list, trimming and dropping blanks and repeats.
pub fn parse_list(value: Option<&str>) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    for item in value.unwrap_or("").split(',').map(str::trim) {
        if !item.is_empty() && !items.iter().any(|seen| seen == item) {
            items.push(item.to_owned());
        }
    }
    items
}

/// Put `relay=` parameters first in an NWC URI.
///
/// NIP-47 does not fix the order, but some clients (routstrd) only accept a
/// URI whose first parameter is `relay=`.
pub fn relay_first(uri: &str) -> String {
    let Some((base, query)) = uri.split_once('?') else {
        return uri.to_owned();
    };
    let (relays, rest): (Vec<&str>, Vec<&str>) = query
        .split('&')
        .partition(|part| part.starts_with("relay="));
    let mut parts = relays;
    parts.extend(rest);
    format!("{base}?{}", parts.join("&"))
}

/// Generate an API key (`sk-pocket-<64 hex>`) and its SHA-256 hex digest.
pub fn new_api_key() -> (String, String) {
    let bytes: [u8; 32] = rand::random();
    let key = format!("sk-pocket-{}", hex::encode(bytes));
    let digest = api_key_digest(&key);
    (key, digest)
}

/// SHA-256 hex digest of an API key.
pub fn api_key_digest(key: &str) -> String {
    use bitcoin::hashes::{sha256, Hash};
    sha256::Hash::hash(key.as_bytes()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_moves_to_front() {
        let uri = "nostr+walletconnect://abc?secret=s&relay=wss%3A%2F%2Fr&lud16=x";
        assert_eq!(
            relay_first(uri),
            "nostr+walletconnect://abc?relay=wss%3A%2F%2Fr&secret=s&lud16=x"
        );
        assert_eq!(
            relay_first("nostr+walletconnect://abc"),
            "nostr+walletconnect://abc"
        );
    }

    #[test]
    fn api_keys_are_unique_and_hashed() {
        let (a, da) = new_api_key();
        let (b, db) = new_api_key();
        assert!(a.starts_with("sk-pocket-") && a.len() == 74);
        assert_ne!(a, b);
        assert_eq!(api_key_digest(&a), da);
        assert_ne!(da, db);
    }

    #[test]
    fn list_parsing_dedupes() {
        assert_eq!(parse_list(Some(" a, b,,a ")), vec!["a", "b"]);
        assert!(parse_list(None).is_empty());
    }

    #[test]
    fn keys_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = keys_path(&dir.path().join("policy.json"));
        assert!(load_keys(&path).expect("empty").is_empty());
        let meta = KeyMeta {
            key_id: "k1".into(),
            label: "app1".into(),
            allowed_models: vec!["m".into()],
            max_per_payment_sats: Some(200),
            created_at: 1,
            api_key_sha256: None,
        };
        save_key(&path, meta.clone()).expect("save");
        save_key(&path, meta.clone()).expect("save again");
        assert_eq!(load_keys(&path).expect("load"), vec![meta]);
    }
}
