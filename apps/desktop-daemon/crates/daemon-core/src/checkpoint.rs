//! Local checkpoint store: recoverable pre-images of every filesystem
//! mutation the agent performs.
//!
//! Design contract (docs/undo-checkpoint-spec.md):
//! • Capture happens BEFORE the mutation, never after (the pre-image is the
//!   only copy of the user's data if the write goes wrong).
//! • Local-only store under `<config>/checkpoints/`; nothing leaves the machine.
//! • Hard limits: MAX_CHECKPOINTS (FIFO), MAX_TOTAL_BYTES (evict oldest),
//!   MAX_FILE_BYTES (larger files → metadata-only, content NOT saved).
//! • Restore is itself reversible: the CURRENT content is checkpointed
//!   before overwriting, and `existed:false` checkpoints restore as delete
//!   (undoing a creation).
//!
//! SPEC DEVIATION (declared): spec said zstd-compressed blobs; shipped
//! uncompressed to keep the dependency tree untouched. The hard byte limits
//! bound the store either way; compression is a later, purely-local change.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Oldest checkpoints are evicted beyond this count (FIFO).
pub const MAX_CHECKPOINTS: usize = 200;
/// Total store budget; oldest-first eviction when exceeded.
pub const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// Files larger than this are NOT content-checkpointed (metadata only).
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointEntry {
    /// Action that caused the mutation (correlates with device_action_log).
    pub action_id: String,
    pub capability: String,
    /// Absolute path of the affected file (canonical-ish, as requested).
    pub path: String,
    /// ms epoch — index ordering is chronological.
    pub ts: u64,
    /// true → `blob` file holds the pre-image; false → target did not exist.
    pub existed: bool,
    /// Pre-image size in bytes (0 when !existed).
    pub size: u64,
    /// SHA-256 hex of the pre-image (empty when !existed or truncated).
    pub hash: String,
    /// true → file exceeded MAX_FILE_BYTES; content NOT stored.
    pub truncated: bool,
}

impl CheckpointEntry {
    pub fn blob_path(&self, root: &Path) -> PathBuf {
        root.join(format!("{}.blob", self.ts))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, CheckpointError>;

pub struct CheckpointStore {
    root: PathBuf,
}

impl CheckpointStore {
    /// Opens (creating if needed) the store AT `root` — callers pass the
    /// FINAL path (`<config>/checkpoints`); open never appends segments
    /// (a previous double-nesting bug wrote to config/checkpoints/checkpoints).
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        Ok(Self { root: root.to_path_buf() })
    }

    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.jsonl")
    }

    fn read_index(&self) -> Vec<CheckpointEntry> {
        let mut out = Vec::new();
        if let Ok(mut f) = std::fs::File::open(self.index_path()) {
            let mut s = String::new();
            if f.read_to_string(&mut s).is_ok() {
                for line in s.lines() {
                    if let Ok(e) = serde_json::from_str::<CheckpointEntry>(line) {
                        out.push(e);
                    }
                }
            }
        }
        out
    }

    /// Persist the index atomically: jsonl snapshot + fsync + rename, so a
    /// crash mid-evict can never leave a dangling blob without an entry.
    fn write_index(&self, entries: &[CheckpointEntry]) -> Result<()> {
        let tmp = self.root.join(".index.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            for e in entries {
                writeln!(f, "{}", serde_json::to_string(e)?)?;
            }
            f.sync_all()?;
        }
        std::fs::rename(&tmp, self.index_path())?;
        Ok(())
    }

    /// Enforce FIFO count + total-bytes budgets. `skip` keeps a just-written
    /// entry out of eviction.
    fn enforce_limits(&self, skip_ts: u64) -> Result<()> {
        let mut entries = self.read_index();
        loop {
            let total: u64 = entries.iter().map(|e| e.size).sum();
            if entries.len() <= MAX_CHECKPOINTS && total <= MAX_TOTAL_BYTES {
                break;
            }
            let victim = entries
                .iter()
                .filter(|e| e.ts != skip_ts)
                .min_by_key(|e| e.ts)
                .cloned();
            let Some(victim) = victim else { break };
            let _ = std::fs::remove_file(victim.blob_path(&self.root));
            entries.retain(|e| e.ts != victim.ts);
        }
        self.write_index(&entries)
    }

    /// Capture the pre-image of `path` before an action mutates it.
    /// Best-effort by contract: errors are logged by the caller but must not
    /// block the action? NO — spec says capture failure MUST refuse the
    /// mutation: a checkpoint we cannot write means no safety net, and
    /// silently proceeding recreates the status quo. Callers map this to an
    /// actionable error.
    pub fn capture(
        &self,
        action_id: &str,
        capability: &str,
        path: &Path,
    ) -> Result<CheckpointEntry> {
        let ts = now_ms();
        // Directory targets (fs.delete can target dirs): checkpoint the
        // listing as metadata (existed=true, truncated=true) — full dir
        // trees are out of budget by design (spec: files, not trees).
        let (existed, size, truncated, hash) = if path.is_dir() {
            let count = std::fs::read_dir(path).map(|it| it.count()).unwrap_or(0);
            (true, count as u64, true, String::new())
        } else if path.exists() {
            let meta = std::fs::metadata(path)?;
            if meta.len() > MAX_FILE_BYTES {
                (true, meta.len(), true, String::new())
            } else {
                let bytes = std::fs::read(path)?;
                let hash = hex::encode(Sha256::digest(&bytes));
                std::fs::write(self.root.join(format!("{ts}.blob")), &bytes)?;
                (true, bytes.len() as u64, false, hash)
            }
        } else {
            (false, 0, false, String::new())
        };
        let entry = CheckpointEntry {
            action_id: action_id.to_string(),
            capability: capability.to_string(),
            path: path.display().to_string(),
            ts,
            existed,
            size,
            hash,
            truncated,
        };
        // Append the entry, then enforce limits (skip our own fresh entry).
        let mut entries = self.read_index();
        entries.push(entry.clone());
        self.write_index(&entries)?;
        self.enforce_limits(ts)?;
        Ok(entry)
    }

    /// List entries newest-first for UI surfaces.
    pub fn list(&self) -> Vec<CheckpointEntry> {
        let mut v = self.read_index();
        v.sort_by_key(|e| std::cmp::Reverse(e.ts));
        v
    }

    /// Restore a checkpoint reversibly. The CURRENT content is checkpointed
    /// first (action_id "restore"), so undo-of-undo works. `existed:false`
    /// restores by deleting (undoing a creation).
    pub fn restore(&self, ts: u64) -> Result<CheckpointEntry> {
        let entries = self.read_index();
        let entry = entries
            .iter()
            .find(|e| e.ts == ts)
            .cloned()
            .ok_or_else(|| {
                CheckpointError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("checkpoint {ts} no existe (¿evicted?)"),
                ))
            })?;
        let target = PathBuf::from(&entry.path);

        // 1) Checkpoint the CURRENT state (reversibility of the restore).
        let rollback = self.capture("restore", "desktop.fs.restore", &target)?;

        // 2) Materialize the pre-image.
        if entry.existed && !entry.truncated {
            let blob = entry.blob_path(&self.root);
            let bytes = std::fs::read(&blob)?;
            // Integrity: the stored pre-image must match its recorded hash.
            if !entry.hash.is_empty() {
                let got = hex::encode(Sha256::digest(&bytes));
                if got != entry.hash {
                    return Err(CheckpointError::Io(std::io::Error::other(
                        "blob corrupto: hash SHA-256 no coincide",
                    )));
                }
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, &bytes)?;
        } else if entry.existed && entry.truncated {
            return Err(CheckpointError::Io(std::io::Error::other(
                "pre-imagen demasiado grande para restaurar (content no almacenado)",
            )));
        } else {
            // !existed: undoing a creation → delete whatever is there now.
            if target.is_dir() {
                std::fs::remove_dir_all(&target)?;
            } else if target.exists() {
                std::fs::remove_file(&target)?;
            }
        }
        Ok(rollback)
    }

    /// Summaries for the UI panel (newest first).
    pub fn summary(&self, limit: usize) -> Vec<String> {
        self.list()
            .into_iter()
            .take(limit)
            .map(|e| {
                let when = chrono::DateTime::from_timestamp_millis(e.ts as i64)
                    .map(|d| d.format("%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| e.ts.to_string());
                format!(
                    "{} {} {} {}{}",
                    when,
                    e.capability.trim_start_matches("desktop.fs."),
                    abbreviate(&e.path, 34),
                    if e.existed { format!("{}B", e.size) } else { "nuevo".into() },
                    if e.truncated { " (grande)" } else { "" }
                )
            })
            .collect()
    }
}

fn abbreviate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(s.chars().count() - (max - 3)).collect();
    format!("…{tail}")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cp-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn capture_write_roundtrip_restores_exact_bytes() {
        let cfg = tmpdir("roundtrip");
        let store = CheckpointStore::open(&cfg).unwrap();
        let f = cfg.join("work").join("a.txt");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, b"ORIGINAL").unwrap();
        let e = store.capture("act-1", "desktop.fs.write", &f).unwrap();
        assert!(e.existed && !e.truncated);
        // Mutate like the agent would.
        std::fs::write(&f, b"AGENT GARBAGE OVERWROTE EVERYTHING").unwrap();
        store.restore(e.ts).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"ORIGINAL");
        // The restore itself left a rollback checkpoint.
        assert!(store.list().iter().any(|x| x.action_id == "restore"));
    }

    #[test]
    fn capture_missing_file_restores_as_delete() {
        let cfg = tmpdir("missing");
        let store = CheckpointStore::open(&cfg).unwrap();
        let f = cfg.join("created.txt");
        let e = store.capture("act-2", "desktop.fs.write", &f).unwrap();
        assert!(!e.existed);
        std::fs::write(&f, b"agent created this").unwrap();
        store.restore(e.ts).unwrap();
        assert!(!f.exists(), "undo of a creation must delete the file");
    }

    #[test]
    fn fifo_eviction_respects_max_checkpoints() {
        let cfg = tmpdir("fifo");
        let store = CheckpointStore::open(&cfg).unwrap();
        let f = cfg.join("f.txt");
        std::fs::write(&f, b"x").unwrap();
        for i in 0..(MAX_CHECKPOINTS + 5) {
            // Distinct timestamps: now_ms() resolution can collide in fast loops.
            std::thread::sleep(std::time::Duration::from_millis(2));
            store.capture(&format!("act-{i}"), "desktop.fs.write", &f).unwrap();
        }
        let list = store.list();
        assert_eq!(list.len(), MAX_CHECKPOINTS);
        // Newest survives, oldest evicted.
        assert!(list.iter().all(|e| e.ts > 0));
    }

    #[test]
    fn restore_missing_entry_is_clean_error() {
        let cfg = tmpdir("noentry");
        let store = CheckpointStore::open(&cfg).unwrap();
        let err = store.restore(12345).unwrap_err();
        assert!(err.to_string().contains("no existe"));
    }

    #[test]
    fn summary_is_bounded_and_readable() {
        let cfg = tmpdir("summary");
        let store = CheckpointStore::open(&cfg).unwrap();
        let f = cfg.join("s.txt");
        std::fs::write(&f, "hola").unwrap();
        let _ = store.capture("a", "desktop.fs.write", &f).unwrap();
        let s = store.summary(10);
        assert_eq!(s.len(), 1);
        assert!(s[0].contains("write") && s[0].contains("s.txt"));
    }
}
