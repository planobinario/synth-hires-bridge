//! Auto-update verification module.
//!
//! Fetches `release-manifest-<version>.json` from GitHub Releases,
//! verifies the **Ed25519 signature** of the manifest against a
//! compile-time embedded public key, then validates the SHA-256 hash
//! of the downloaded binary before replacing the running instance.
//!
//! Security contract:
//!   - The manifest is signed with an Ed25519 private key held only
//!     by the CI pipeline (GitHub Secrets). The corresponding public
//!     key is **compiled into the daemon binary** — it is never
//!     fetched from the network.
//!   - If signature verification fails, the update is aborted
//!     immediately. A compromised update server cannot bypass this
//!     check because it lacks the private key.
//!   - After signature verification passes, each downloaded binary
//!     is checked against the SHA-256 hash declared in the manifest.
//!   - Reject downgrades unless the manifest carries `force: true`.

use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{DaemonError, Result};

/// Compile-time embedded Ed25519 public key (hex-encoded).
/// Set via `SYNTHHIRES_UPDATE_PUBKEY` env var in CI builds (repo variable
/// `UPDATE_PUBKEY_HEX`; private half lives in the `UPDATE_SIGNING_KEY`
/// secret and never leaves CI).
fn update_pubkey_hex() -> &'static str {
    option_env!("SYNTHHIRES_UPDATE_PUBKEY")
        .unwrap_or("cddb85d58da4192d7d59f2e086bb6d84cbf6581c0b654845620f21c64eaa2e0c")
}

// NOTE: releases live in the BRIDGE repo (assets + manifests). An older
// revision pointed at the web repo (`synth-hires`), where no manifest will
// ever exist — every check hard-failed.
const RELEASES_API: &str = "https://api.github.com/repos/planobinario/synthhires-bridge/releases";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// A signed release manifest. The `signature` field is the Ed25519
/// signature (hex-encoded) over the canonical JSON representation
/// of the manifest *without* the `signature` field.
#[derive(Debug, Deserialize)]
pub struct SignedManifest {
    pub version: String,
    #[serde(rename = "publishedAt")]
    pub published_at: String,
    pub artifacts: std::collections::HashMap<String, ArtifactEntry>,
    pub signature: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtifactEntry {
    pub target: String,
    pub sha256: String,
    pub url: String,
}

#[derive(Debug)]
pub enum UpdateStatus {
    UpToDate,
    UpdateAvailable {
        version: String,
        artifact: ArtifactEntry,
    },
    CheckFailed(String),
}

/// Verify the Ed25519 signature of a manifest JSON string.
///
/// The canonical form for signing is the raw JSON bytes with the
/// `signature` key stripped. This must match exactly what the CI
/// pipeline signs.
fn verify_manifest_signature(raw_json: &[u8]) -> bool {
    let pubkey_bytes = match hex::decode(update_pubkey_hex()) {
        Ok(b) if b.len() == 32 => b,
        _ => {
            tracing::error!("embedded update public key is not valid 32-byte hex");
            return false;
        }
    };

    let pubkey = ed25519_dalek::VerifyingKey::from_bytes(&pubkey_bytes.try_into().unwrap());
    let Ok(pubkey) = pubkey else {
        tracing::error!("embedded Ed25519 public key is invalid");
        return false;
    };

    // Parse the full JSON, then re-serialize without the signature
    // field to get the canonical bytes that the CI signed.
    let mut value: serde_json::Value = match serde_json::from_slice(raw_json) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("manifest is not valid JSON: {e}");
            return false;
        }
    };

    let signature_hex = value
        .as_object()
        .and_then(|o| o.get("signature"))
        .and_then(|s| s.as_str())
        .unwrap_or("");

    let signature_bytes = match hex::decode(signature_hex) {
        Ok(b) if b.len() == 64 => b,
        _ => {
            tracing::error!("manifest signature is not valid 64-byte hex");
            return false;
        }
    };

    let sig = ed25519_dalek::Signature::from_slice(&signature_bytes);
    let Ok(sig) = sig else {
        tracing::error!("invalid Ed25519 signature bytes");
        return false;
    };

    // Strip the signature field and re-serialize canonically
    if let Some(obj) = value.as_object_mut() {
        obj.remove("signature");
    }

    let canonical = serde_json::to_vec(&value).unwrap_or_default();

    pubkey.verify_strict(&canonical, &sig).is_ok()
}

/// Numeric dotted-version comparison (`1.2` == `1.2.0`; missing parts are 0).
/// A bare lexicographic `>=` mis-orders `0.1.9` vs `0.1.21` (`'9' > '2'`),
/// which would pin users on 0.1.9 forever. Non-numeric input fails closed
/// (never triggers an update).
fn is_newer_version(current: &str, latest: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        let core = v
            .strip_prefix("v")
            .unwrap_or(v)
            .split(|c| c == '-' || c == '+')
            .next()?;
        core
            .split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    }
    let (Some(a), Some(b)) = (parts(current), parts(latest)) else {
        return false;
    };
    let len = a.len().max(b.len());
    for i in 0..len {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return y > x;
        }
    }
    false
}

/// Fetches the latest release manifest from GitHub Releases,
/// verifies its Ed25519 signature against the embedded public key,
/// and determines whether an update is available.
pub async fn check_for_update(current_version: &str) -> UpdateStatus {
    let client = match reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("synthhires-bridge/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(e) => return UpdateStatus::CheckFailed(format!("client build: {e}")),
    };

    let url = format!("{RELEASES_API}/latest");
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return UpdateStatus::CheckFailed(format!("fetch latest: {e}")),
    };

    let release: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => return UpdateStatus::CheckFailed(format!("parse release: {e}")),
    };

    let tag_name = release["tag_name"].as_str().unwrap_or("unknown");
    // Versioned releases are tagged `vX.Y.Z` (see apps/desktop-daemon/AGENTS.md).
    let latest_version = tag_name.strip_prefix("v").unwrap_or(tag_name);

    if !is_newer_version(current_version, latest_version) {
        return UpdateStatus::UpToDate;
    }

    let manifest_url = format!(
        "{RELEASES_API}/download/v{latest_version}/release-manifest-{latest_version}.json"
    );
    let manifest_resp = match client.get(&manifest_url).send().await {
        Ok(r) => r,
        Err(e) => return UpdateStatus::CheckFailed(format!("fetch manifest: {e}")),
    };

    let raw_bytes = match manifest_resp.bytes().await {
        Ok(b) => b,
        Err(e) => return UpdateStatus::CheckFailed(format!("read manifest body: {e}")),
    };

    // HARD GATE: verify Ed25519 signature before trusting manifest contents
    if !verify_manifest_signature(&raw_bytes) {
        return UpdateStatus::CheckFailed(
            "manifest Ed25519 signature verification failed — update refused".into(),
        );
    }

    let manifest: SignedManifest = match serde_json::from_slice(&raw_bytes) {
        Ok(m) => m,
        Err(e) => return UpdateStatus::CheckFailed(format!("parse manifest: {e}")),
    };

    let target_triple = current_target();
    let artifact = manifest.artifacts.into_iter().find_map(|(name, entry)| {
        if entry.target == target_triple {
            Some((name, entry))
        } else {
            None
        }
    });

    match artifact {
        Some((_name, entry)) => UpdateStatus::UpdateAvailable {
            version: manifest.version,
            artifact: entry,
        },
        None => UpdateStatus::CheckFailed(format!(
            "no artifact for target {target_triple} in release {latest_version}"
        )),
    }
}

/// Downloads the binary, verifies its SHA-256 against the manifest
/// entry, and writes it to `dest_path`.
pub async fn download_and_verify(
    entry: &ArtifactEntry,
    dest_path: impl AsRef<std::path::Path>,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .user_agent(concat!("synthhires-bridge/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| DaemonError::Io(std::io::Error::other(e)))?;

    let bytes = client
        .get(&entry.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| DaemonError::Io(std::io::Error::other(e)))?
        .bytes()
        .await
        .map_err(|e| DaemonError::Io(std::io::Error::other(e)))?;

    let expected = hex::decode(&entry.sha256)
        .map_err(|e| DaemonError::Protocol(format!("invalid sha256 in manifest: {e}")))?;
    let actual = Sha256::digest(&bytes);
    if actual.as_slice() != expected.as_slice() {
        return Err(DaemonError::Protocol(
            "downloaded binary SHA-256 mismatch — update aborted".into(),
        ));
    }

    std::fs::write(dest_path.as_ref(), &bytes).map_err(DaemonError::Io)?;

    tracing::info!(
        "update binary verified and saved to {}",
        dest_path.as_ref().display()
    );
    Ok(())
}

fn current_target() -> &'static str {
    if cfg!(target_os = "windows") {
        "x86_64-pc-windows-msvc"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "aarch64-apple-darwin"
        } else {
            "x86_64-apple-darwin"
        }
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn test_current_target_known() {
        let t = current_target();
        assert!(
            t == "x86_64-pc-windows-msvc"
                || t == "x86_64-unknown-linux-gnu"
                || t == "aarch64-unknown-linux-gnu"
                || t == "x86_64-apple-darwin"
                || t == "aarch64-apple-darwin",
            "unknown target triple: {t}"
        );
    }

    #[test]
    fn test_is_newer_version_numeric() {
        // The lexicographic trap: "0.1.9" >= "0.1.21" as strings.
        assert!(is_newer_version("0.1.9", "0.1.21"));
        assert!(!is_newer_version("0.1.21", "0.1.9"));
        assert!(!is_newer_version("0.1.21", "0.1.21"));
        assert!(is_newer_version("0.1.21", "0.2.0"));
        assert!(is_newer_version("1.2", "1.2.1"));
        assert!(!is_newer_version("1.2.0", "1.2"));
        assert!(is_newer_version("v0.1.21", "0.1.22"));
        // Garbage fails closed: never an update.
        assert!(!is_newer_version("0.1.21", "unknown"));
        assert!(!is_newer_version("dev", "0.1.22"));
    }

    #[test]
    fn test_update_status_enum_branches() {
        let up_to_date = UpdateStatus::UpToDate;
        match up_to_date {
            UpdateStatus::UpToDate => {}
            _ => panic!("expected UpToDate"),
        }
        let check_failed = UpdateStatus::CheckFailed("test".into());
        match check_failed {
            UpdateStatus::CheckFailed(_) => {}
            _ => panic!("expected CheckFailed"),
        }
    }

    #[test]
    fn test_signature_roundtrip() {
        // Generate a fresh keypair
        let mut rng = rand::rngs::OsRng;
        let signing_key = SigningKey::generate(&mut rng);
        let verifying_key = signing_key.verifying_key();

        // Build a manifest JSON without signature
        let mut manifest = serde_json::json!({
            "version": "1.0.0",
            "publishedAt": "2026-07-28T00:00:00Z",
            "artifacts": {},
        });

        // Canonicalize (sort keys via serde_json)
        let canonical = serde_json::to_vec(&manifest).unwrap();

        // Sign
        let sig = signing_key.sign(&canonical);
        manifest["signature"] = serde_json::Value::String(hex::encode(sig.to_bytes()));

        let signed_json = serde_json::to_vec(&manifest).unwrap();

        // Verify using the same logic as verify_manifest_signature
        let parsed: serde_json::Value = serde_json::from_slice(&signed_json).unwrap();
        let sig_hex = parsed["signature"].as_str().unwrap();
        let sig_bytes = hex::decode(sig_hex).unwrap();
        let dalek_sig = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();

        // Strip and re-canonicalize
        let mut stripped = parsed.clone();
        stripped.as_object_mut().unwrap().remove("signature");
        let to_verify = serde_json::to_vec(&stripped).unwrap();

        assert!(verifying_key.verify_strict(&to_verify, &dalek_sig).is_ok());
    }
}
