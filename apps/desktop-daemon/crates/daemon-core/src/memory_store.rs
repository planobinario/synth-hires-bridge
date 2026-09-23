//! Persistent project-scoped memory (feature: memory) — OMP-style curated
//! memory, backed by the same bundled SQLite the chat store uses.
//!
//! The model curates its own long-term knowledge per project root:
//! - `remember`  → store a fact (dedup by content hash; rescaling importance)
//! - `recall`    → keyword search (FTS5 when available, LIKE fallback)
//! - `forget`    → delete by id or clear a whole project
//! - `reflect`   → write a `reflection` entry summarizing lessons learned
//! - `stats`     → counts per project for the agent's own hygiene
//!
//! Design notes:
//! - Storage lives next to chats.db in the daemon config dir:
//!   `~/.config/synthhires-bridge/memory.db`. One file, all projects.
//! - `project` is an opaque string key (the web passes the workspace root;
//!   we hash it for the FTS external-content table id but keep the raw path
//!   indexed too).
//! - Scores: `importance` (1..=5) decays nothing by itself; recall ranks by
//!   recency * importance so useful facts surface first.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

type MemResult<T> = Result<T, String>;

// ─── Request / result payloads (camelCase over the wire, like the rest) ──────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", tag = "op")]
pub enum MemoryOp {
    #[serde(rename = "remember")]
    Remember {
        project: String,
        content: String,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        importance: Option<i64>,
        #[serde(default)]
        tags: Vec<String>,
    },
    #[serde(rename = "recall")]
    Recall {
        project: String,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        limit: Option<i64>,
    },
    #[serde(rename = "forget")]
    Forget {
        project: String,
        #[serde(default)]
        id: Option<i64>,
        #[serde(default)]
        #[allow(dead_code)]
        all: bool,
    },
    #[serde(rename = "reflect")]
    Reflect {
        project: String,
        summary: String,
        #[serde(default)]
        tags: Vec<String>,
    },
    #[serde(rename = "stats")]
    #[allow(dead_code)]
    Stats { project: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntry {
    pub id: i64,
    pub kind: String,
    pub content: String,
    pub importance: i64,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub hits: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryWriteResult {
    pub id: i64,
    pub deduplicated: bool,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecallResult {
    pub entries: Vec<MemoryEntry>,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryForgetResult {
    pub deleted: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryStatsResult {
    pub total: i64,
    pub reflections: i64,
}

// ─── Store ───────────────────────────────────────────────────────────────────

pub struct MemoryStore {
    /// Same shape as ChatStore: rusqlite is Send but !Sync, and the dispatch
    /// runs ops inside spawn_blocking — the mutex makes the store Sync.
    conn: std::sync::Mutex<Connection>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn content_hash(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    let bytes = h.finalize();
    let mut out = String::with_capacity(64);
    for b in bytes.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Parse comma-separated tags. Empty → [].
fn parse_tags(raw: Option<String>) -> Vec<String> {
    raw.map(|t| {
        t.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

impl MemoryStore {
    pub fn open(path: PathBuf) -> MemResult<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(&path).map_err(|e| format!("open: {e}"))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("wal: {e}"))?;
        let store = Self {
            conn: std::sync::Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Fallback when the config dir is unwritable: the agent keeps working,
    /// memory just doesn't survive daemon restarts.
    pub fn open_in_memory() -> MemResult<Self> {
        let conn = Connection::open_in_memory().map_err(|e| format!("open_in_memory: {e}"))?;
        let store = Self {
            conn: std::sync::Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn lock(&self) -> MemResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|e| format!("lock: {e}"))
    }

    pub fn default_path() -> PathBuf {
        directories::ProjectDirs::from("com", "synthhires", "bridge")
            .map(|d| d.config_dir().join("memory.db"))
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("synthhires-bridge")
                    .join("memory.db")
            })
    }

    fn migrate(&self) -> MemResult<()> {
        let conn = self.lock()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT 'fact',
                content TEXT NOT NULL,
                importance INTEGER NOT NULL DEFAULT 3,
                tags TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                hits INTEGER NOT NULL DEFAULT 0
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_memories_dedup
                ON memories(project, content_hash);
            CREATE INDEX IF NOT EXISTS idx_memories_project
                ON memories(project, updated_at DESC);
            "#,
        )
        .map_err(|e| format!("migrate: {e}"))?;
        // FTS5 is bundled in rusqlite; create the virtual table but tolerate
        // builds without it (LIKE fallback keeps recall working regardless).
        // NOTE: reuse the `conn` guard already held by this function — locking
        // twice would deadlock (std Mutex is not reentrant).
        let fts = conn
            .execute_batch(
                r#"
                CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                    content, project UNINDEXED, content='memories', content_rowid='id'
                );
                CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                    INSERT INTO memories_fts(rowid, content, project)
                        VALUES (new.id, new.content, new.project);
                END;
                CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                    INSERT INTO memories_fts(memories_fts, rowid, content, project)
                        VALUES ('delete', old.id, old.content, old.project);
                END;
                CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE OF content ON memories BEGIN
                    INSERT INTO memories_fts(memories_fts, rowid, content, project)
                        VALUES ('delete', old.id, old.content, old.project);
                    INSERT INTO memories_fts(rowid, content, project)
                        VALUES (new.id, new.content, new.project);
                END;
                "#,
            )
            .is_ok();
        let _ = fts; // availability recorded implicitly by first recall attempt
        Ok(())
    }

    fn fts_available(&self) -> bool {
        let Ok(conn) = self.conn.lock() else {
            return false;
        };
        conn.query_row("SELECT count(*) FROM memories_fts", [], |r| r.get::<_, i64>(0))
            .is_ok()
    }

    pub fn execute(&self, op: MemoryOp) -> MemResult<MemoryResult> {
        match op {
            MemoryOp::Remember {
                project,
                content,
                kind,
                importance,
                tags,
            } => self.remember(project, content, kind, importance, tags),
            MemoryOp::Recall {
                project,
                query,
                limit,
            } => self.recall(project, query, limit),
            MemoryOp::Forget { project, id, all } => self.forget(project, id, all),
            MemoryOp::Reflect {
                project,
                summary,
                tags,
            } => {
                let kind = Some("reflection".to_string());
                let importance = Some(4);
                let r = self.remember(project, summary, kind, importance, tags)?;
                match r {
                    MemoryResult::Write(w) => Ok(MemoryResult::Write(w)),
                    _ => Err("reflect: unexpected result".into()),
                }
            }
            MemoryOp::Stats { project } => self.stats(project),
        }
    }

    fn remember(
        &self,
        project: String,
        content: String,
        kind: Option<String>,
        importance: Option<i64>,
        tags: Vec<String>,
    ) -> MemResult<MemoryResult> {
        let content = content.trim().to_string();
        if content.is_empty() {
            return Err("remember: content is empty".into());
        }
        let kind = kind
            .filter(|k| matches!(*k, ref s if !s.trim().is_empty()))
            .unwrap_or_else(|| "fact".to_string());
        let importance = importance.unwrap_or(3).clamp(1, 5);
        let tags_csv = tags.join(",");
        let hash = content_hash(&content);
        let t = now();
        let conn = self.lock()?;
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM memories WHERE project = ?1 AND content_hash = ?2",
                params![project, hash],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| format!("dedup lookup: {e}"))?;
        if let Some(id) = existing {
            // Re-remembering an existing fact bumps importance and recency.
            conn.execute(
                "UPDATE memories
                 SET importance = MIN(5, importance + 1), updated_at = ?3, hits = hits + 1
                 WHERE id = ?1 AND project = ?2",
                params![id, project, t],
            )
            .map_err(|e| format!("bump: {e}"))?;
            let total: i64 = conn
                .query_row(
                    "SELECT count(*) FROM memories WHERE project = ?1",
                    params![project],
                    |r| r.get(0),
                )
                .map_err(|e| format!("count: {e}"))?;
            return Ok(MemoryResult::Write(MemoryWriteResult {
                id,
                deduplicated: true,
                total,
            }));
        }
        conn.execute(
            "INSERT INTO memories
                 (project, content_hash, kind, content, importance, tags, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![project, hash, kind, content, importance, tags_csv, t],
        )
        .map_err(|e| format!("insert: {e}"))?;
        let id = conn.last_insert_rowid();
        let total: i64 = conn
            .query_row(
                "SELECT count(*) FROM memories WHERE project = ?1",
                params![project],
                |r| r.get(0),
            )
            .map_err(|e| format!("count: {e}"))?;
        Ok(MemoryResult::Write(MemoryWriteResult {
            id,
            deduplicated: false,
            total,
        }))
    }

    fn recall(
        &self,
        project: String,
        query: Option<String>,
        limit: Option<i64>,
    ) -> MemResult<MemoryResult> {
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let empty = query.as_deref().unwrap_or("").trim().is_empty();
        // Rank: recency (half-life feel via updated_at) * importance, computed
        // in SQL so it works on both the FTS and LIKE paths.
        let rank_sql = "importance * 10 + (updated_at / 86400)";
        // Decide the strategy BEFORE locking: the empty-terms path recurses.
        let strategy: std::result::Result<String, String> = if empty {
            Ok("all".into())
        } else if self.fts_available() {
            // Sanitize the query into prefix terms: foo bar → "foo"* "bar"*
            let terms: Vec<String> = query
                .as_deref()
                .unwrap_or("")
                .split_whitespace()
                .map(|t| format!("\"{}\"*", t.replace('"', "")))
                .filter(|t| t.len() > 2)
                .collect();
            if terms.is_empty() {
                return self.recall(project, None, Some(limit));
            }
            Ok(format!("fts:{}", terms.join(" ")))
        } else {
            let like = format!(
                "%{}%",
                query.as_deref().unwrap_or("").replace('%', "").replace('_', "")
            );
            Ok(format!("like:{like}"))
        };
        let strategy = strategy?;
        let conn = self.lock()?;
        // Each strategy runs in its own scope so `stmt` (which borrows conn)
        // is dropped before the hits update below re-borrows conn.
        let rows: Vec<MemoryEntry> = {
            let select_columns =
                "id, kind, content, importance, tags, created_at, updated_at, hits";
            if strategy == "all" {
                let sql = format!(
                    "SELECT {select_columns} FROM memories WHERE project = ?1
                     ORDER BY {rank_sql} DESC, updated_at DESC LIMIT ?2"
                );
                let mut stmt = conn
                    .prepare(&sql)
                    .map_err(|e| format!("prepare: {e}"))?;
                let mapped: Vec<MemoryEntry> = stmt
                    .query_map(params![project, limit], row_to_entry)
                    .map_err(|e| format!("query: {e}"))?
                    .filter_map(|r| r.ok())
                    .collect();
                mapped
            } else if let Some(fts_query) = strategy.strip_prefix("fts:") {
                let sql = format!(
                    "SELECT m.id, m.kind, m.content, m.importance, m.tags,
                            m.created_at, m.updated_at, m.hits
                     FROM memories_fts f JOIN memories m ON m.id = f.rowid
                     WHERE memories_fts MATCH ?1 AND m.project = ?2
                     ORDER BY {rank_sql} DESC, m.updated_at DESC LIMIT ?3"
                );
                let mut stmt = conn
                    .prepare(&sql)
                    .map_err(|e| format!("prepare fts: {e}"))?;
                let mapped: Vec<MemoryEntry> = stmt
                    .query_map(params![fts_query, project, limit], row_to_entry)
                    .map_err(|e| format!("query fts: {e}"))?
                    .filter_map(|r| r.ok())
                    .collect();
                mapped
            } else if let Some(like) = strategy.strip_prefix("like:") {
                let sql = format!(
                    "SELECT {select_columns} FROM memories
                     WHERE project = ?1 AND content LIKE ?2
                     ORDER BY {rank_sql} DESC, updated_at DESC LIMIT ?3"
                );
                let mut stmt = conn
                    .prepare(&sql)
                    .map_err(|e| format!("prepare: {e}"))?;
                let mapped: Vec<MemoryEntry> = stmt
                    .query_map(params![project, like, limit], row_to_entry)
                    .map_err(|e| format!("query: {e}"))?
                    .filter_map(|r| r.ok())
                    .collect();
                mapped
            } else {
                unreachable!("strategy")
            }
        };
        // Touch hits for surfaced entries (usage signal for later curation).
        for e in &rows {
            let _ = conn.execute(
                "UPDATE memories SET hits = hits + 1 WHERE id = ?1",
                params![e.id],
            );
        }
        let total: i64 = conn
            .query_row(
                "SELECT count(*) FROM memories WHERE project = ?1",
                params![project],
                |r| r.get(0),
            )
            .map_err(|e| format!("count: {e}"))?;
        Ok(MemoryResult::Recall(MemoryRecallResult { entries: rows, total }))
    }

    fn forget(&self, project: String, id: Option<i64>, all: bool) -> MemResult<MemoryResult> {
        let conn = self.lock()?;
        let deleted: i64 = if all {
            conn.execute("DELETE FROM memories WHERE project = ?1", params![project])
                .map_err(|e| format!("delete: {e}"))? as i64
        } else if let Some(id) = id {
            conn.execute(
                "DELETE FROM memories WHERE id = ?1 AND project = ?2",
                params![id, project],
            )
            .map_err(|e| format!("delete: {e}"))? as i64
        } else {
            return Err("forget: needs id or all=true".into());
        };
        Ok(MemoryResult::Forget(MemoryForgetResult { deleted }))
    }

    fn stats(&self, project: String) -> MemResult<MemoryResult> {
        let conn = self.lock()?;
        let (total, reflections): (i64, i64) = conn
            .query_row(
                "SELECT count(*),
                        sum(CASE WHEN kind = 'reflection' THEN 1 ELSE 0 END)
                 FROM memories WHERE project = ?1",
                params![project],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
            )
            .map_err(|e| format!("stats: {e}"))?;
        Ok(MemoryResult::Stats(MemoryStatsResult { total, reflections }))
    }
}

fn row_to_entry(
    r: &rusqlite::Row<'_>,
) -> std::result::Result<MemoryEntry, rusqlite::Error> {
    Ok(MemoryEntry {
        id: r.get(0)?,
        kind: r.get(1)?,
        content: r.get(2)?,
        importance: r.get(3)?,
        tags: parse_tags(r.get(4)?),
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        hits: r.get(7)?,
    })
}

/// Tagged so the dispatch match returns one serde_json::Value.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MemoryResult {
    Write(MemoryWriteResult),
    Recall(MemoryRecallResult),
    Forget(MemoryForgetResult),
    Stats(MemoryStatsResult),
}

// ─── Tests (tempfile-style: unique paths under the OS temp dir) ──────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "shmem-{}-{}-{}.db",
            tag,
            std::process::id(),
            now() + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as i64
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn remember_dedups_and_bumps() {
        let store = MemoryStore::open(tmp_db("dedup")).unwrap();
        let op = |c: &str| MemoryOp::Remember {
            project: "/repo".into(),
            content: c.into(),
            kind: None,
            importance: Some(3),
            tags: vec![],
        };
        let first = match store.execute(op("usa pnpm, no npm")).unwrap() {
            MemoryResult::Write(w) => w,
            _ => panic!("write expected"),
        };
        assert!(!first.deduplicated);
        let second = match store.execute(op("usa pnpm, no npm")).unwrap() {
            MemoryResult::Write(w) => w,
            _ => panic!("write expected"),
        };
        assert!(second.deduplicated);
        assert_eq!(second.id, first.id);
        assert_eq!(second.total, 1);
    }

    #[test]
    fn recall_ranks_and_hits() {
        let store = MemoryStore::open(tmp_db("recall")).unwrap();
        for content in [
            "the deploy script lives in scripts/deploy.sh",
            "CI runs vitest before cargo test",
            "postgres port is 5433 on the staging box",
        ] {
            let _ = store.execute(MemoryOp::Remember {
                project: "/repo".into(),
                content: content.into(),
                kind: None,
                importance: Some(3),
                tags: vec![],
            });
        }
        let res = match store
            .execute(MemoryOp::Recall {
                project: "/repo".into(),
                query: Some("deploy".into()),
                limit: None,
            })
            .unwrap()
        {
            MemoryResult::Recall(r) => r,
            _ => panic!("recall expected"),
        };
        assert_eq!(res.entries.len(), 1);
        assert!(res.entries[0].content.contains("deploy.sh"));
        // Empty query lists everything (ranked by recency*importance).
        let all = match store
            .execute(MemoryOp::Recall {
                project: "/repo".into(),
                query: None,
                limit: None,
            })
            .unwrap()
        {
            MemoryResult::Recall(r) => r,
            _ => panic!("recall expected"),
        };
        assert_eq!(all.entries.len(), 3);
        assert_eq!(all.total, 3);
        assert!(all.entries[0].hits >= 1); // recall touched hits
    }

    #[test]
    fn reflect_creates_reflection_kind() {
        let store = MemoryStore::open(tmp_db("reflect")).unwrap();
        let _ = store
            .execute(MemoryOp::Reflect {
                project: "/repo".into(),
                summary: "Prefer editing over rewriting; verify with cargo test.".into(),
                tags: vec!["lesson".into()],
            })
            .unwrap();
        let stats = match store
            .execute(MemoryOp::Stats {
                project: "/repo".into(),
            })
            .unwrap()
        {
            MemoryResult::Stats(s) => s,
            _ => panic!("stats expected"),
        };
        assert_eq!(stats.total, 1);
        assert_eq!(stats.reflections, 1);
    }

    #[test]
    fn forget_by_id_and_all_is_project_scoped() {
        let store = MemoryStore::open(tmp_db("forget")).unwrap();
        let remember = |project: &str, c: &str| {
            match store
                .execute(MemoryOp::Remember {
                    project: project.into(),
                    content: c.into(),
                    kind: None,
                    importance: None,
                    tags: vec![],
                })
                .unwrap()
            {
                MemoryResult::Write(w) => w,
                _ => panic!("write expected"),
            }
        };
        let a = remember("/repo-a", "fact a1");
        let _b = remember("/repo-a", "fact a2");
        let _c = remember("/repo-b", "fact b1");
        let res = match store
            .execute(MemoryOp::Forget {
                project: "/repo-a".into(),
                id: Some(a.id),
                all: false,
            })
            .unwrap()
        {
            MemoryResult::Forget(f) => f,
            _ => panic!("forget expected"),
        };
        assert_eq!(res.deleted, 1);
        // Other project untouched by all=true on repo-a
        let res = match store
            .execute(MemoryOp::Forget {
                project: "/repo-a".into(),
                id: None,
                all: true,
            })
            .unwrap()
        {
            MemoryResult::Forget(f) => f,
            _ => panic!("forget expected"),
        };
        assert_eq!(res.deleted, 1); // a2 only
        let stats = match store
            .execute(MemoryOp::Stats {
                project: "/repo-b".into(),
            })
            .unwrap()
        {
            MemoryResult::Stats(s) => s,
            _ => panic!("stats expected"),
        };
        assert_eq!(stats.total, 1);
    }
}
