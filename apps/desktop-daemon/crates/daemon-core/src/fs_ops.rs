//! Filesystem operations.
//!
//! Read, write, delete — all gated by `CapabilityGate`. Writes go
//! through a temp file + atomic rename so a crash mid-write never
//! leaves a corrupted file behind.

use crate::{
    capability::{CapabilityGate, GateDecision},
    DaemonError, Result,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::{Path, PathBuf};
use tokio::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct FsReadRequest {
    pub path: PathBuf,
    #[serde(default)]
    pub max_bytes: Option<u64>,
    /// When true, the content comes back annotated as `NNNN | hash` per
    /// line so the model can address hashline patches (feature: fs.patch).
    #[serde(default)]
    pub annotate: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsReadResult {
    pub content_base64: String,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FsWriteRequest {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsWriteResult {
    pub bytes_written: u64,
    pub verified: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FsDeleteRequest {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FsVerifyRequest {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsVerifyResult {
    pub exists: bool,
    pub is_dir: bool,
    pub readable: bool,
    pub writable: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FsListRequest {
    /// Directory to list. When absent/empty, return the OS roots
    /// (drive letters on Windows, `/` + home on Unix).
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsListResult {
    pub parent: Option<String>,
    pub entries: Vec<FsEntry>,
}

pub struct FsOps<'a> {
    gate: &'a CapabilityGate,
}

impl<'a> FsOps<'a> {
    pub fn new(gate: &'a CapabilityGate) -> Self {
        Self { gate }
    }

    pub async fn read(&self, req: FsReadRequest) -> Result<FsReadResult> {
        self.gate_for_path("desktop.fs.read", &req.path)?;
        let max = req.max_bytes.unwrap_or(1_048_576).min(10_485_760);
        let bytes = fs::read(&req.path).await.map_err(DaemonError::Io)?;
        let truncated = if bytes.len() as u64 > max {
            &bytes[..max as usize]
        } else {
            &bytes[..]
        };
        Ok(FsReadResult {
            content_base64: base64_encode(truncated),
            size: truncated.len() as u64,
        })
    }

    pub async fn write(&self, req: FsWriteRequest) -> Result<FsWriteResult> {
        self.gate_for_path("desktop.fs.write", &req.path)?;
        let parent = req
            .path
            .parent()
            .ok_or_else(|| DaemonError::PathDenied(format!("{}: no parent", req.path.display())))?;
        fs::create_dir_all(parent).await.map_err(DaemonError::Io)?;
        let tmp = req.path.with_extension("synthhires-tmp");
        fs::write(&tmp, req.content.as_bytes())
            .await
            .map_err(DaemonError::Io)?;
        fs::rename(&tmp, &req.path).await.map_err(DaemonError::Io)?;
        // Empirical verification: read back what we just wrote. Only a
        // byte-identical file is reported as verified. This is the same
        // honesty guarantee as the web-side local tools.
        let written = fs::read(&req.path).await.map_err(DaemonError::Io)?;
        let verified = written == req.content.as_bytes();
        Ok(FsWriteResult {
            bytes_written: written.len() as u64,
            verified,
        })
    }

    pub async fn delete(&self, req: FsDeleteRequest) -> Result<()> {
        self.gate_for_path("desktop.fs.delete", &req.path)?;
        let meta = fs::metadata(&req.path).await.map_err(DaemonError::Io)?;
        if meta.is_dir() {
            fs::remove_dir_all(&req.path)
                .await
                .map_err(DaemonError::Io)?;
        } else {
            fs::remove_file(&req.path).await.map_err(DaemonError::Io)?;
        }
        Ok(())
    }

    /// Empirical access verification: the agent asks "can you really
    /// work in this folder?" and the daemon proves it on disk. Returns
    /// a full per-axis report instead of guessing:
    ///   • exists   — the path resolves on this machine
    ///   • is_dir   — it is a directory (the expected shape)
    ///   • readable — a directory listing works
    ///   • writable — a probe file was created AND read back AND
    ///                removed; the probe never touches user files.
    /// Failure never throws — every axis degrades to a field so the
    /// web UI can explain exactly what's wrong.
    pub async fn verify(&self, req: FsVerifyRequest) -> FsVerifyResult {
        match fs::metadata(&req.path).await {
            Err(e) => FsVerifyResult {
                exists: false,
                is_dir: false,
                readable: false,
                writable: false,
                error: Some(format!("no existe o no es accesible: {e}")),
            },
            Ok(meta) => {
                let is_dir = meta.is_dir();
                let readable = if is_dir {
                    fs::read_dir(&req.path).await.is_ok()
                } else {
                    fs::read(&req.path).await.is_ok()
                };
                let writable = if is_dir {
                    self.probe_write(&req.path).await
                } else {
                    let parent = req.path.parent().unwrap_or_else(|| {
                        std::path::Path::new(&req.path)
                            .parent()
                            .unwrap_or(std::path::Path::new("."))
                    });
                    self.probe_write(parent).await
                };
                FsVerifyResult {
                    exists: true,
                    is_dir,
                    readable,
                    writable,
                    error: if readable && writable {
                        None
                    } else {
                        Some(format!(
                            "lectura: {}, escritura: {}",
                            if readable { "ok" } else { "FALLA" },
                            if writable { "ok" } else { "FALLA" },
                        ))
                    },
                }
            }
        }
    }

    /// Create a unique probe file, read it back byte-for-byte, then
    /// remove it. Any step failing leaves `writable=false` — and never
    /// leaves the probe behind.
    async fn probe_write(&self, dir: &std::path::Path) -> bool {
        let probe_name = format!(".synthhires-verify-{}.tmp", uuid::Uuid::new_v4());
        let probe = dir.join(probe_name);
        let payload = format!("synthhires-verify:{}", uuid::Uuid::new_v4());
        if fs::write(&probe, payload.as_bytes()).await.is_err() {
            return false;
        }
        let read_back = match fs::read(&probe).await {
            Ok(b) => b == payload.as_bytes(),
            Err(_) => false,
        };
        let _ = fs::remove_file(&probe).await;
        read_back
    }

    /// Directory browser for the workspace picker. Read-only and gated by
    /// the capability grant alone (like `verify`, it runs BEFORE a path is
    /// attached, so it must not require the path to already be in
    /// alwaysAllowPaths). `path=None` returns the OS roots.
    pub async fn list(&self, req: FsListRequest) -> Result<FsListResult> {
        let target = req.path.as_deref().unwrap_or("").trim();
        if target.is_empty() {
            return Ok(FsListResult {
                parent: None,
                entries: self.roots().await,
            });
        }
        let dir = Path::new(target);
        let mut entries: Vec<FsEntry> = Vec::new();
        let mut rd = fs::read_dir(dir).await.map_err(DaemonError::Io)?;
        while let Some(entry) = rd.next_entry().await.map_err(DaemonError::Io)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            entries.push(FsEntry {
                name,
                path: path.display().to_string(),
                is_dir,
            });
        }
        // Dirs first, then files; case-insensitive name within each group.
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        let parent = dir.parent().and_then(|p| {
            let s = p.to_string_lossy().into_owned();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        });
        Ok(FsListResult { parent, entries })
    }

    async fn roots(&self) -> Vec<FsEntry> {
        #[cfg(target_os = "windows")]
        {
            let mut out = Vec::new();
            for letter in b'A'..=b'Z' {
                let root = format!("{}:\\", letter as char);
                if std::path::Path::new(&root).exists() {
                    out.push(FsEntry {
                        name: root.clone(),
                        path: root,
                        is_dir: true,
                    });
                }
            }
            out
        }
        #[cfg(not(target_os = "windows"))]
        {
            let mut out = vec![FsEntry {
                name: "/".to_string(),
                path: "/".to_string(),
                is_dir: true,
            }];
            if let Some(home) = std::env::var_os("HOME") {
                let home = home.to_string_lossy().into_owned();
                if !home.is_empty() {
                    out.push(FsEntry {
                        name: home.clone(),
                        path: home,
                        is_dir: true,
                    });
                }
            }
            out
        }
    }

    fn gate_for_path(&self, capability: &str, path: &Path) -> Result<()> {
        // Mutaciones de disco atraviesan el chequeo real (symlinks
        // resueltos): un dir dentro del root que apunte fuera degrada a
        // RequireConsent en vez de Allow lexical. Lecturas/listados no
        // cambian de semántica (el consentimiento de fs.read ya es
        // per-acción cuando no hay always-allow).
        let real_check = capability != "desktop.fs.read";
        let decision = if real_check {
            self.gate
                .check_path_real(capability, path)
                .map_err(|e| {
                    DaemonError::CapabilityDenied(format!(
                        "{}: no se pudo resolver la ruta real: {e}",
                        path.display()
                    ))
                })?
        } else {
            self.gate.check_path(capability, path)
        };
        match decision {
            GateDecision::Allow => Ok(()),
            GateDecision::RequireConsent => Err(DaemonError::CapabilityDenied(format!(
                "{} requires consent for {}",
                capability,
                path.display()
            ))),
            GateDecision::Deny => Err(DaemonError::CapabilityDenied(capability.into())),
        }
    }
}

/// Minimal base64 encoder so we don't pull in the `base64` crate.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
        i += 3;
    }
    if i < bytes.len() {
        let n = (bytes[i] as u32) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        if i + 1 < bytes.len() {
            let n = n | ((bytes[i + 1] as u32) << 8);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        } else {
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
    }
    out
}

// ─── Hashline patching (feature: fs.patch) ───────────────────────────────
//
// OMP-style content-hash anchors. The web reads a file as `NNNN | line`,
// the model returns edits pointing at those line anchors plus a short hash
// of the line(s) being replaced. Before applying ANY edit we re-hash the
// current on-disk lines: a stale anchor (file changed since the read, or a
// previous patch in the same batch moved the lines) rejects the WHOLE patch
// atomically — nothing is written. That is the guarantee full-content
// rewrite cannot give and naive find/replace keeps failing at.

pub const HASHLINE_PREFIX: &str = " | ";

/// Short content hash shown to the model: first 6 hex chars of SHA-256.
/// Collisions at 24 bits are rare and harmless: a collision only widens
/// the match candidates, and uniqueness is enforced per-target anyway.
pub fn short_hash(line: &str) -> String {
    let digest = sha2::Sha256::digest(line.as_bytes());
    hex::encode(&digest[..3])
}

#[cfg(test)]
fn hashline_lineno(line: &str) -> Option<usize> {
    let (num, _rest) = line.split_once(HASHLINE_PREFIX)?;
    num.parse::<usize>().ok()
}

#[derive(Debug, Clone, Deserialize)]
pub struct HashlineEdit {
    /// 1-indexed line number the edit REPLACES (from the annotated read).
    pub start_line: usize,
    /// Inclusive end line. Defaults to start_line (single-line edit).
    #[serde(default)]
    pub end_line: Option<usize>,
    /// Comma-separated short hashes the model saw for those lines. Optional
    /// for compatibility; when absent the anchor degrades to line-number-only.
    #[serde(default)]
    pub hashes: Option<String>,
    /// Replacement lines. Empty slice deletes the range.
    #[serde(default)]
    pub replacement: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FsPatchRequest {
    pub path: PathBuf,
    pub edits: Vec<HashlineEdit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsPatchApplied {
    pub start_line: usize,
    pub end_line: usize,
    pub lines_added: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsPatchResult {
    pub bytes_written: u64,
    pub verified: bool,
    pub applied: Vec<FsPatchApplied>,
}

fn anchor_matches(actual: &[String], expected: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    expected
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .all(|h| actual.iter().any(|a| a == h))
}

fn annotate_content(content: &str) -> String {
    let mut out = String::with_capacity(content.len() + content.lines().count() * 12);
    for (idx, line) in content.lines().enumerate() {
        out.push_str(&format!(
            "{}{}{}\n",
            idx + 1,
            HASHLINE_PREFIX,
            short_hash(line)
        ));
    }
    out
}

impl<'a> FsOps<'a> {
    /// Read with `NNNN | hash` annotation, or plain content when requested.
    pub async fn read_annotated(&self, req: FsReadRequest, annotate: bool) -> Result<FsReadResult> {
        let mut result = self.read(req).await?;
        if annotate {
            let plain = base64_decode(&result.content_base64).ok_or_else(|| {
                DaemonError::Protocol("fs.read: base64 roundtrip failed".into())
            })?;
            let text = String::from_utf8_lossy(&plain);
            result.content_base64 = base64_encode(annotate_content(&text).as_bytes());
        }
        Ok(result)
    }

    /// Apply a batch of hashline edits atomically: every anchor is verified
    /// against the on-disk content BEFORE anything is written. One stale
    /// anchor fails the whole request with the current hashes so the model
    /// can recover in the next turn. Never writes on failure.
    pub async fn patch(&self, req: FsPatchRequest) -> Result<FsPatchResult> {
        self.gate_for_path("desktop.fs.write", &req.path)?;
        if req.edits.is_empty() {
            return Err(DaemonError::Protocol("fs.patch: no edits".into()));
        }
        let original_bytes = fs::read(&req.path).await.map_err(DaemonError::Io)?;
        let original = String::from_utf8_lossy(&original_bytes);
        let mut lines: Vec<String> = original.lines().map(ToOwned::to_owned).collect();

        // Validate all anchors first (positions refer to the ORIGINAL file).
        let mut planned: Vec<(usize, usize, Vec<String>)> = Vec::with_capacity(req.edits.len());
        for edit in &req.edits {
            let start = edit.start_line.max(1);
            let end = edit.end_line.unwrap_or(edit.start_line).max(start);
            if end > lines.len() {
                return Err(DaemonError::Protocol(format!(
                    "stale_anchor: end_line {} beyond EOF ({} lines). Re-read the file.",
                    end,
                    lines.len()
                )));
            }
            let actual: Vec<String> = lines[start - 1..end]
                .iter()
                .map(|l| short_hash(l))
                .collect();
            if !anchor_matches(&actual, edit.hashes.as_deref()) {
                return Err(DaemonError::Protocol(PatchAnchorError {
                    start_line: start,
                    end_line: end,
                    expected_hashes: edit.hashes.clone(),
                    actual_hashes: actual,
                }
                .to_string()));
            }
            planned.push((start - 1, end, edit.replacement.clone()));
        }

        // Apply bottom-up so earlier (upper) anchors keep their positions.
        planned.sort_by(|a, b| b.0.cmp(&a.0));
        let mut applied: Vec<FsPatchApplied> = Vec::with_capacity(planned.len());
        let mut last_start: Option<usize> = None;
        for (idx, end, replacement) in planned {
            if let Some(prev) = last_start {
                if end > prev {
                    return Err(DaemonError::Protocol(
                        "fs.patch: overlapping edit ranges".into(),
                    ));
                }
            }
            last_start = Some(idx);
            let lines_added = replacement.len();
            lines.splice(idx..end, replacement);
            applied.push(FsPatchApplied {
                start_line: idx + 1,
                end_line: end,
                lines_added,
            });
        }
        applied.reverse();

        let mut content = lines.join("\n");
        if original_bytes.ends_with(b"\n") {
            content.push('\n');
        }
        let tmp = req.path.with_extension("synthhires-tmp");
        fs::write(&tmp, content.as_bytes()).await.map_err(DaemonError::Io)?;
        if fs::rename(&tmp, &req.path).await.is_err() {
            let _ = fs::remove_file(&tmp).await;
            return Err(DaemonError::Io(std::io::Error::other(
                "patch rename failed",
            )));
        }
        let written = fs::read(&req.path).await.map_err(DaemonError::Io)?;
        let verified = written == content.as_bytes();
        Ok(FsPatchResult {
            bytes_written: written.len() as u64,
            verified,
            applied,
        })
    }
}

/// Stale-anchor diagnostic returned to the model when an edit would no
/// longer land where it was cut from.
#[derive(Debug)]
pub struct PatchAnchorError {
    pub start_line: usize,
    pub end_line: usize,
    pub expected_hashes: Option<String>,
    pub actual_hashes: Vec<String>,
}

impl std::fmt::Display for PatchAnchorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stale_anchor: lines {}-{} changed since read (expected {:?}, now {:?}). Re-read the file and re-emit the patch.",
            self.start_line, self.end_line, self.expected_hashes, self.actual_hashes
        )
    }
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [255u8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let bytes: Vec<u8> = input
        .bytes()
        .filter(|b| !b" \t\r\n".contains(b))
        .collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut chunk = [0u8; 4];
    let mut filled = 0usize;
    for &b in &bytes {
        if b == b'=' {
            break;
        }
        let v = table[b as usize];
        if v == 255 {
            return None;
        }
        chunk[filled] = v;
        filled += 1;
        if filled == 4 {
            out.push((chunk[0] << 2) | (chunk[1] >> 4));
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
            out.push((chunk[2] << 6) | chunk[3]);
            filled = 0;
        }
    }
    match filled {
        0 => {}
        2 => out.push((chunk[0] << 2) | (chunk[1] >> 4)),
        3 => {
            out.push((chunk[0] << 2) | (chunk[1] >> 4));
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
        }
        _ => return None,
    }
    Some(out)
}

#[cfg(test)]
mod hashline_tests {
    use super::*;
    use crate::capability::{CapabilityGate, ScopeSnapshot};

    fn test_gate(dir: &std::path::Path) -> CapabilityGate {
        let snap = ScopeSnapshot {
            capabilities: vec!["desktop.fs.read".into(), "desktop.fs.write".into()],
            always_allow_paths: vec![dir.to_path_buf()],
        };
        CapabilityGate::new(snap)
    }

    fn decode(result: &FsReadResult) -> String {
        String::from_utf8(base64_decode(&result.content_base64).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn annotate_and_patch_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sh-patch-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("a.txt");
        tokio::fs::write(&path, "alpha\nbeta\ngamma\n").await.unwrap();
        let gate = test_gate(&dir);
        let ops = FsOps::new(&gate);

        // 1. Annotated read shows the same hashes the patch will verify.
        let annotated = ops
            .read_annotated(
                FsReadRequest { path: path.clone(), max_bytes: None, annotate: Some(true) },
                true,
            )
            .await
            .unwrap();
        let text = decode(&annotated);
        assert!(text.contains(&format!("1{}{}", HASHLINE_PREFIX, short_hash("alpha"))));
        assert!(text.contains(&format!("2{}{}", HASHLINE_PREFIX, short_hash("beta"))));

        // 2. Valid patch replaces line 2 and matches the annotated hash.
        let result = ops
            .patch(FsPatchRequest {
                path: path.clone(),
                edits: vec![HashlineEdit {
                    start_line: 2,
                    end_line: None,
                    hashes: Some(short_hash("beta")),
                    replacement: vec!["beta-prime".into()],
                }],
            })
            .await
            .unwrap();
        assert_eq!(result.applied.len(), 1);
        assert!(result.verified);
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(after, "alpha\nbeta-prime\ngamma\n");

        // 3. Stale anchor (old hash) rejects BEFORE writing.
        let err = ops
            .patch(FsPatchRequest {
                path: path.clone(),
                edits: vec![HashlineEdit {
                    start_line: 2,
                    end_line: None,
                    hashes: Some(short_hash("beta")),
                    replacement: vec!["x".into()],
                }],
            })
            .await;
        assert!(err.is_err());
        let unchanged = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(unchanged, "alpha\nbeta-prime\ngamma\n");

        // 4. Line-number-only anchor still works (hashes=None).
        ops.patch(FsPatchRequest {
            path: path.clone(),
            edits: vec![HashlineEdit {
                start_line: 1,
                end_line: None,
                hashes: None,
                replacement: vec!["ALPHA".into()],
            }],
        })
        .await
        .unwrap();
        let final_text = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(final_text.starts_with("ALPHA\n"));

        // 5. Bottom-up multi-edit: insert+delete in one atomic batch.
        tokio::fs::write(&path, "one\ntwo\nthree\nfour\n").await.unwrap();
        let h_two = short_hash("two");
        let h_four = short_hash("four");
        let multi = ops
            .patch(FsPatchRequest {
                path: path.clone(),
                edits: vec![
                    HashlineEdit {
                        start_line: 2,
                        end_line: None,
                        hashes: Some(h_two),
                        replacement: vec!["dos-a".into(), "dos-b".into()],
                    },
                    HashlineEdit {
                        start_line: 4,
                        end_line: None,
                        hashes: Some(h_four),
                        replacement: vec![],
                    },
                ],
            })
            .await
            .unwrap();
        assert_eq!(multi.applied.len(), 2);
        let after_multi = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(after_multi, "one\ndos-a\ndos-b\nthree\n");

        // 6. Overlapping ranges rejected.
        tokio::fs::write(&path, "a\nb\nc\n").await.unwrap();
        let overlap = ops
            .patch(FsPatchRequest {
                path: path.clone(),
                edits: vec![
                    HashlineEdit { start_line: 1, end_line: Some(2), hashes: None, replacement: vec!["x".into()] },
                    HashlineEdit { start_line: 2, end_line: Some(3), hashes: None, replacement: vec!["y".into()] },
                ],
            })
            .await;
        assert!(overlap.is_err());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn hashline_lineno_parses() {
        assert_eq!(hashline_lineno("42 | a1b2c3"), Some(42));
        assert_eq!(hashline_lineno("1 | 000000"), Some(1));
        assert_eq!(hashline_lineno("no prefix"), None);
        assert_eq!(hashline_lineno("| hash"), None);
        assert_eq!(hashline_lineno("x | 1a2b3c"), None);
    }

    #[test]
    fn base64_roundtrip_matches_encoder() {
        for sample in ["", "a", "ab", "abc", "abcd", "\u{1F600} emoji"] {
            let encoded = base64_encode(sample.as_bytes());
            let decoded = base64_decode(&encoded).unwrap();
            assert_eq!(decoded, sample.as_bytes());
        }
    }
}
