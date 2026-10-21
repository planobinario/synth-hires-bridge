//! Local chat store for the desktop daemon.
//!
//! Owns the user's local conversation archive at
//! `~/.config/synthhires-bridge/chats.db` (SQLite, WAL). The web app
//! pushes conversation snapshots over the bridge (`sync.chat.push`);
//! this module persists them and exposes read/export APIs for the
//! daemon UI's "Conversaciones" tab.
//!
//! BYOK boundary: API keys NEVER enter this file. Only conversation
//! metadata + message text are stored, exactly what the web app
//! already keeps in IndexedDB / Postgres.
//!
//! Conflict resolution: last-write-wins on `updated_at`, mirroring
//! the web app's `/api/sync/delta` semantics.

use daemon_protocol::{ChatSyncArtifact, ChatSyncConversation, ChatSyncMessage, ChatSyncProject};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const _SCHEMA_VERSION: i64 = 2;

pub struct ChatStore {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct StoredConversation {
    pub id: String,
    pub title: Option<String>,
    pub workspace_ref: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub is_pinned: bool,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub created_at: i64,
}

impl ChatStore {
    pub fn open(db_path: &Path) -> Result<Self, String> {
        if let Some(dir) = db_path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
        }
        let conn = Connection::open(db_path).map_err(|e| format!("open sqlite: {e}"))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("wal: {e}"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| format!("fk: {e}"))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn default_path() -> PathBuf {
        directories::ProjectDirs::from("com", "synthhires", "bridge")
            .map(|d| d.config_dir().join("chats.db"))
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("synthhires-bridge")
                    .join("chats.db")
            })
    }

    fn migrate(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {e}"))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_meta (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );
            "#,
        )
        .map_err(|e| format!("meta table: {e}"))?;

        let current: Option<i64> = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("read version: {e}"))?;

        // v2 adds opt-in sync tables (artifacts + projects). Runs after v1
        // so existing chats.db files migrate without data loss.
        if current.unwrap_or(0) < 2 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS local_artifacts (
                    id TEXT PRIMARY KEY,
                    conversation_id TEXT,
                    title TEXT,
                    kind TEXT,
                    content TEXT NOT NULL DEFAULT '',
                    updated_at INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS idx_artifacts_conv ON local_artifacts(conversation_id, updated_at);
                CREATE TABLE IF NOT EXISTS local_projects (
                    id TEXT PRIMARY KEY,
                    name TEXT,
                    description TEXT,
                    updated_at INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO schema_meta (key, value) VALUES ('schema_version', 2)
                  ON CONFLICT(key) DO UPDATE SET value = MAX(value, 2);
                "#,
            )
            .map_err(|e| format!("v2 migration: {e}"))?;
        }
        if current.unwrap_or(0) < 1 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS local_conversations (
                    id TEXT PRIMARY KEY,
                    title TEXT,
                    workspace_ref TEXT,
                    model TEXT,
                    provider TEXT,
                    is_pinned INTEGER NOT NULL DEFAULT 0,
                    updated_at INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS local_messages (
                    id TEXT PRIMARY KEY,
                    conversation_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    created_at INTEGER NOT NULL DEFAULT 0,
                    FOREIGN KEY(conversation_id) REFERENCES local_conversations(id) ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS idx_messages_conv ON local_messages(conversation_id, created_at);
                INSERT INTO schema_meta (key, value) VALUES ('schema_version', 1)
                  ON CONFLICT(key) DO UPDATE SET value = 1;
                "#,
            )
            .map_err(|e| format!("v1 migration: {e}"))?;
        }
        Ok(())
    }

    /// Upsert a full conversation snapshot. LWW on `updated_at` for the
    /// conversation row; messages are upserted individually and stale
    /// messages not present in the snapshot are deleted (the snapshot is
    /// authoritative for that conversation).
    pub fn upsert_conversation(&self, conv: &ChatSyncConversation) -> Result<usize, String> {
        let mut conn = self.conn.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = conn.transaction().map_err(|e| format!("tx: {e}"))?;

        let updated_at = conv.updated_at.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        }) as i64;

        let existing_updated: Option<i64> = tx
            .query_row(
                "SELECT updated_at FROM local_conversations WHERE id = ?1",
                [&conv.id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("read conv: {e}"))?;

        if let Some(existing) = existing_updated {
            if existing > updated_at {
                // Remote snapshot is stale; keep local copy.
                return Ok(0);
            }
        }

        let workspace_ref = conv
            .workspace_ref
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();

        tx.execute(
            r#"
            INSERT INTO local_conversations (id, title, workspace_ref, model, provider, is_pinned, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                workspace_ref = excluded.workspace_ref,
                model = excluded.model,
                provider = excluded.provider,
                is_pinned = excluded.is_pinned,
                updated_at = excluded.updated_at
            "#,
            params![
                conv.id,
                conv.title.clone().unwrap_or_else(|| "New conversation".into()),
                if workspace_ref.is_empty() { None } else { Some(workspace_ref) },
                conv.model.clone().unwrap_or_else(|| "gpt-4o".into()),
                conv.provider.clone().unwrap_or_else(|| "openai".into()),
                conv.is_pinned.unwrap_or(false) as i64,
                updated_at,
            ],
        )
        .map_err(|e| format!("upsert conv: {e}"))?;

        let mut inserted = 0usize;
        {
            let mut stmt = tx
                .prepare(
                    r#"
                    INSERT INTO local_messages (id, conversation_id, role, content, created_at)
                    VALUES (?1, ?2, ?3, ?4, ?5)
                    ON CONFLICT(id) DO UPDATE SET
                        role = excluded.role,
                        content = excluded.content,
                        created_at = excluded.created_at
                    "#,
                )
                .map_err(|e| format!("prepare msg: {e}"))?;
            for m in &conv.messages {
                let content = normalise_content(&m.content);
                stmt.execute(params![
                    m.id,
                    conv.id,
                    m.role,
                    content,
                    m.created_at.unwrap_or(0) as i64,
                ])
                .map_err(|e| format!("upsert msg {}: {e}", m.id))?;
                inserted += 1;
            }
        }

        // Snapshot is authoritative: drop local messages missing from it.
        if !conv.messages.is_empty() {
            let keep: Vec<&String> = conv.messages.iter().map(|m| &m.id).collect();
            let placeholders = vec!["?"; keep.len()].join(",");
            let sql = format!(
                "DELETE FROM local_messages WHERE conversation_id = ?1 AND id NOT IN ({placeholders})"
            );
            let mut params_owned: Vec<Box<dyn rusqlite::ToSql>> =
                Vec::with_capacity(keep.len() + 1);
            params_owned.push(Box::new(conv.id.clone()));
            for id in keep {
                params_owned.push(Box::new(id.clone()));
            }
            let refs: Vec<&dyn rusqlite::ToSql> = params_owned.iter().map(|b| b.as_ref()).collect();
            tx.execute(&sql, refs.as_slice())
                .map_err(|e| format!("prune msgs: {e}"))?;
        }

        tx.commit().map_err(|e| format!("commit: {e}"))?;
        Ok(inserted)
    }

    pub fn list_conversations(&self, limit: usize) -> Result<Vec<StoredConversation>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, title, workspace_ref, model, provider, is_pinned, updated_at
                 FROM local_conversations ORDER BY updated_at DESC LIMIT ?1",
            )
            .map_err(|e| format!("prepare list: {e}"))?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                Ok(StoredConversation {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    workspace_ref: row.get(2)?,
                    model: row.get(3)?,
                    provider: row.get(4)?,
                    is_pinned: row.get::<_, i64>(5)? != 0,
                    updated_at: row.get(6)?,
                })
            })
            .map_err(|e| format!("list: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("collect list: {e}"))
    }

    pub fn search_conversations(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<StoredConversation>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {e}"))?;
        let pattern = format!("%{}%", query);
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT c.id, c.title, c.workspace_ref, c.model, c.provider, c.is_pinned, c.updated_at
                 FROM local_conversations c
                 LEFT JOIN local_messages m ON m.conversation_id = c.id
                 WHERE c.title LIKE ?1 OR m.content LIKE ?1
                 ORDER BY c.updated_at DESC LIMIT ?2",
            )
            .map_err(|e| format!("prepare search: {e}"))?;
        let rows = stmt
            .query_map(params![pattern, limit as i64], |row| {
                Ok(StoredConversation {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    workspace_ref: row.get(2)?,
                    model: row.get(3)?,
                    provider: row.get(4)?,
                    is_pinned: row.get::<_, i64>(5)? != 0,
                    updated_at: row.get(6)?,
                })
            })
            .map_err(|e| format!("search: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("collect search: {e}"))
    }

    pub fn get_messages(&self, conversation_id: &str) -> Result<Vec<StoredMessage>, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, conversation_id, role, content, created_at
                 FROM local_messages WHERE conversation_id = ?1 ORDER BY created_at ASC, id ASC",
            )
            .map_err(|e| format!("prepare msgs: {e}"))?;
        let rows = stmt
            .query_map([conversation_id], |row| {
                Ok(StoredMessage {
                    id: row.get(0)?,
                    conversation_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| format!("msgs: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("collect msgs: {e}"))
    }

    pub fn count_conversations(&self) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {e}"))?;
        conn.query_row("SELECT count(*) FROM local_conversations", [], |r| r.get(0))
            .map_err(|e| format!("count: {e}"))
    }

    pub fn count_messages(&self) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {e}"))?;
        conn.query_row("SELECT count(*) FROM local_messages", [], |r| r.get(0))
            .map_err(|e| format!("count msgs: {e}"))
    }

    /// Upsert an artifact snapshot (LWW on updated_at, content capped at 200k chars).
    pub fn upsert_artifact(&self, a: &ChatSyncArtifact) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {e}"))?;
        let updated_at = a.updated_at.unwrap_or(0) as i64;
        let existing: Option<i64> = conn
            .query_row("SELECT updated_at FROM local_artifacts WHERE id = ?1", [&a.id], |r| r.get(0))
            .optional()
            .map_err(|e| format!("read artifact: {e}"))?;
        if let Some(cur) = existing {
            if cur > updated_at {
                return Ok(());
            }
        }
        let content: String = a.content.chars().take(200_000).collect();
        conn.execute(
            r#"INSERT INTO local_artifacts (id, conversation_id, title, kind, content, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)
               ON CONFLICT(id) DO UPDATE SET conversation_id=excluded.conversation_id, title=excluded.title,
               kind=excluded.kind, content=excluded.content, updated_at=excluded.updated_at"#,
            params![a.id, a.conversation_id, a.title, a.kind, content, updated_at],
        )
        .map_err(|e| format!("upsert artifact: {e}"))?;
        Ok(())
    }

    /// Upsert a project snapshot (LWW on updated_at).
    pub fn upsert_project(&self, p: &ChatSyncProject) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {e}"))?;
        let updated_at = p.updated_at.unwrap_or(0) as i64;
        let existing: Option<i64> = conn
            .query_row("SELECT updated_at FROM local_projects WHERE id = ?1", [&p.id], |r| r.get(0))
            .optional()
            .map_err(|e| format!("read project: {e}"))?;
        if let Some(cur) = existing {
            if cur > updated_at {
                return Ok(());
            }
        }
        conn.execute(
            r#"INSERT INTO local_projects (id, name, description, updated_at)
               VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT(id) DO UPDATE SET name=excluded.name,
               description=excluded.description, updated_at=excluded.updated_at"#,
            params![p.id, p.name, p.description, updated_at],
        )
        .map_err(|e| format!("upsert project: {e}"))?;
        Ok(())
    }

    pub fn delete_conversation(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| format!("lock: {e}"))?;
        let n = conn
            .execute("DELETE FROM local_conversations WHERE id = ?1", [id])
            .map_err(|e| format!("delete: {e}"))?;
        Ok(n > 0)
    }
}

fn normalise_content(content: &str) -> String {
    content.chars().take(200_000).collect()
}

#[allow(dead_code)]
fn _keep_sync_type_imported(_m: &ChatSyncMessage) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "synthhires-chat-store-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("chats.db")
    }

    #[test]
    fn upsert_and_read_roundtrip() {
        let path = temp_db();
        let store = ChatStore::open(&path).unwrap();
        let conv = ChatSyncConversation {
            id: "conv-1".into(),
            title: Some("Test".into()),
            workspace_ref: None,
            model: Some("gpt-4o".into()),
            provider: Some("openai".into()),
            is_pinned: Some(false),
            updated_at: Some(1000),
            messages: vec![
                ChatSyncMessage {
                    id: "m1".into(),
                    role: "user".into(),
                    content: "hola".into(),
                    created_at: Some(1),
                },
                ChatSyncMessage {
                    id: "m2".into(),
                    role: "assistant".into(),
                    content: "hola de vuelta".into(),
                    created_at: Some(2),
                },
            ],
        };
        let inserted = store.upsert_conversation(&conv).unwrap();
        assert_eq!(inserted, 2);
        assert_eq!(store.count_conversations().unwrap(), 1);
        assert_eq!(store.count_messages().unwrap(), 2);
        let msgs = store.get_messages("conv-1").unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        let convs = store.list_conversations(10).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title.as_deref(), Some("Test"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn stale_snapshot_does_not_overwrite() {
        let path = temp_db();
        let store = ChatStore::open(&path).unwrap();
        let newer = ChatSyncConversation {
            id: "c".into(),
            title: Some("newer".into()),
            workspace_ref: None,
            model: None,
            provider: None,
            is_pinned: None,
            updated_at: Some(2000),
            messages: vec![],
        };
        store.upsert_conversation(&newer).unwrap();
        let stale = ChatSyncConversation {
            id: "c".into(),
            title: Some("stale".into()),
            workspace_ref: None,
            model: None,
            provider: None,
            is_pinned: None,
            updated_at: Some(1000),
            messages: vec![],
        };
        store.upsert_conversation(&stale).unwrap();
        let convs = store.list_conversations(10).unwrap();
        assert_eq!(convs[0].title.as_deref(), Some("newer"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn snapshot_prunes_missing_messages() {
        let path = temp_db();
        let store = ChatStore::open(&path).unwrap();
        let first = ChatSyncConversation {
            id: "c".into(),
            title: None,
            workspace_ref: None,
            model: None,
            provider: None,
            is_pinned: None,
            updated_at: Some(1000),
            messages: vec![
                ChatSyncMessage {
                    id: "m1".into(),
                    role: "user".into(),
                    content: "a".into(),
                    created_at: Some(1),
                },
                ChatSyncMessage {
                    id: "m2".into(),
                    role: "user".into(),
                    content: "b".into(),
                    created_at: Some(2),
                },
            ],
        };
        store.upsert_conversation(&first).unwrap();
        let second = ChatSyncConversation {
            id: "c".into(),
            title: None,
            workspace_ref: None,
            model: None,
            provider: None,
            is_pinned: None,
            updated_at: Some(2000),
            messages: vec![ChatSyncMessage {
                id: "m1".into(),
                role: "user".into(),
                content: "a'".into(),
                created_at: Some(1),
            }],
        };
        store.upsert_conversation(&second).unwrap();
        assert_eq!(store.count_messages().unwrap(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
