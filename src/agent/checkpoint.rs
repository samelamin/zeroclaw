use crate::providers::traits::ChatMessage;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// A snapshot of conversation state at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub session_id: String,
    pub label: Option<String>,
    pub history: Vec<ChatMessage>,
    pub turn_count: usize,
    pub metadata: Option<serde_json::Value>,
    pub created_at: String,
}

/// Storage backend for conversation checkpoints.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn save(&self, checkpoint: &Checkpoint) -> anyhow::Result<String>;
    async fn load(&self, id: &str) -> anyhow::Result<Option<Checkpoint>>;
    async fn list(&self, session_id: &str) -> anyhow::Result<Vec<Checkpoint>>;
    async fn delete(&self, id: &str) -> anyhow::Result<bool>;
    async fn clear_session(&self, session_id: &str) -> anyhow::Result<usize>;
}

/// Create a new checkpoint with a generated ID and timestamp.
pub fn create_checkpoint(
    session_id: &str,
    label: Option<&str>,
    history: &[ChatMessage],
    metadata: Option<serde_json::Value>,
) -> Checkpoint {
    Checkpoint {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session_id.to_string(),
        label: label.map(|s| s.to_string()),
        history: history.to_vec(),
        turn_count: history.len(),
        metadata,
        created_at: chrono::Utc::now().to_rfc3339(),
    }
}

// ---------------------------------------------------------------------------
// InMemoryCheckpointStore
// ---------------------------------------------------------------------------

/// In-memory checkpoint store for testing.
pub struct InMemoryCheckpointStore {
    checkpoints: Mutex<Vec<Checkpoint>>,
}

impl InMemoryCheckpointStore {
    pub fn new() -> Self {
        Self {
            checkpoints: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl CheckpointStore for InMemoryCheckpointStore {
    async fn save(&self, checkpoint: &Checkpoint) -> anyhow::Result<String> {
        let mut store = self.checkpoints.lock().unwrap();
        if let Some(pos) = store.iter().position(|c| c.id == checkpoint.id) {
            store[pos] = checkpoint.clone();
        } else {
            store.push(checkpoint.clone());
        }
        Ok(checkpoint.id.clone())
    }

    async fn load(&self, id: &str) -> anyhow::Result<Option<Checkpoint>> {
        let store = self.checkpoints.lock().unwrap();
        Ok(store.iter().find(|c| c.id == id).cloned())
    }

    async fn list(&self, session_id: &str) -> anyhow::Result<Vec<Checkpoint>> {
        let store = self.checkpoints.lock().unwrap();
        let mut result: Vec<Checkpoint> = store
            .iter()
            .filter(|c| c.session_id == session_id)
            .cloned()
            .collect();
        result.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(result)
    }

    async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let mut store = self.checkpoints.lock().unwrap();
        let len_before = store.len();
        store.retain(|c| c.id != id);
        Ok(store.len() < len_before)
    }

    async fn clear_session(&self, session_id: &str) -> anyhow::Result<usize> {
        let mut store = self.checkpoints.lock().unwrap();
        let len_before = store.len();
        store.retain(|c| c.session_id != session_id);
        Ok(len_before - store.len())
    }
}

// ---------------------------------------------------------------------------
// SqliteCheckpointStore
// ---------------------------------------------------------------------------

/// SQLite-backed checkpoint store.
pub struct SqliteCheckpointStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl SqliteCheckpointStore {
    /// Create a new store, initializing the schema on the provided connection.
    pub fn new(conn: Arc<Mutex<rusqlite::Connection>>) -> anyhow::Result<Self> {
        {
            let db = conn.lock().unwrap();
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS checkpoints (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    label TEXT,
                    history TEXT NOT NULL,
                    turn_count INTEGER NOT NULL,
                    metadata TEXT,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_checkpoints_session ON checkpoints(session_id);
                CREATE INDEX IF NOT EXISTS idx_checkpoints_created ON checkpoints(created_at);",
            )?;
        }
        Ok(Self { conn })
    }
}

#[async_trait]
impl CheckpointStore for SqliteCheckpointStore {
    async fn save(&self, checkpoint: &Checkpoint) -> anyhow::Result<String> {
        let cp = checkpoint.clone();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let db = conn.lock().unwrap();
            let history_json = serde_json::to_string(&cp.history)?;
            let metadata_json = cp.metadata.as_ref().map(serde_json::to_string).transpose()?;
            db.execute(
                "INSERT OR REPLACE INTO checkpoints (id, session_id, label, history, turn_count, metadata, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    cp.id,
                    cp.session_id,
                    cp.label,
                    history_json,
                    cp.turn_count,
                    metadata_json,
                    cp.created_at,
                ],
            )?;
            Ok(cp.id)
        })
        .await?
    }

    async fn load(&self, id: &str) -> anyhow::Result<Option<Checkpoint>> {
        let id = id.to_string();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let db = conn.lock().unwrap();
            use rusqlite::OptionalExtension;
            let result = db
                .query_row(
                    "SELECT id, session_id, label, history, turn_count, metadata, created_at
                     FROM checkpoints WHERE id = ?1",
                    rusqlite::params![id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, usize>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, String>(6)?,
                        ))
                    },
                )
                .optional()?;

            match result {
                Some((
                    id,
                    session_id,
                    label,
                    history_json,
                    turn_count,
                    metadata_json,
                    created_at,
                )) => {
                    let history: Vec<ChatMessage> = serde_json::from_str(&history_json)?;
                    let metadata: Option<serde_json::Value> = metadata_json
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?;
                    Ok(Some(Checkpoint {
                        id,
                        session_id,
                        label,
                        history,
                        turn_count,
                        metadata,
                        created_at,
                    }))
                }
                None => Ok(None),
            }
        })
        .await?
    }

    async fn list(&self, session_id: &str) -> anyhow::Result<Vec<Checkpoint>> {
        let session_id = session_id.to_string();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let db = conn.lock().unwrap();
            let mut stmt = db.prepare(
                "SELECT id, session_id, label, history, turn_count, metadata, created_at
                 FROM checkpoints WHERE session_id = ?1 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(rusqlite::params![session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, usize>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?;

            let mut checkpoints = Vec::new();
            for row in rows {
                let (id, session_id, label, history_json, turn_count, metadata_json, created_at) =
                    row?;
                let history: Vec<ChatMessage> = serde_json::from_str(&history_json)?;
                let metadata: Option<serde_json::Value> = metadata_json
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?;
                checkpoints.push(Checkpoint {
                    id,
                    session_id,
                    label,
                    history,
                    turn_count,
                    metadata,
                    created_at,
                });
            }
            Ok(checkpoints)
        })
        .await?
    }

    async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let db = conn.lock().unwrap();
            let affected = db.execute(
                "DELETE FROM checkpoints WHERE id = ?1",
                rusqlite::params![id],
            )?;
            Ok(affected > 0)
        })
        .await?
    }

    async fn clear_session(&self, session_id: &str) -> anyhow::Result<usize> {
        let session_id = session_id.to_string();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let db = conn.lock().unwrap();
            let affected = db.execute(
                "DELETE FROM checkpoints WHERE session_id = ?1",
                rusqlite::params![session_id],
            )?;
            Ok(affected)
        })
        .await?
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::ChatMessage;

    fn sample_history() -> Vec<ChatMessage> {
        vec![
            ChatMessage::user("Hello"),
            ChatMessage::assistant("Hi there!"),
        ]
    }

    // ---- InMemory tests ----

    #[tokio::test]
    async fn save_and_load_checkpoint() {
        let store = InMemoryCheckpointStore::new();
        let cp = create_checkpoint("s1", Some("step-1"), &sample_history(), None);
        let id = store.save(&cp).await.unwrap();

        let loaded = store
            .load(&id)
            .await
            .unwrap()
            .expect("checkpoint should exist");
        assert_eq!(loaded.id, cp.id);
        assert_eq!(loaded.session_id, "s1");
        assert_eq!(loaded.label.as_deref(), Some("step-1"));
        assert_eq!(loaded.history.len(), 2);
        assert_eq!(loaded.turn_count, 2);
    }

    #[tokio::test]
    async fn list_checkpoints_ordered_newest_first() {
        let store = InMemoryCheckpointStore::new();

        let mut cp1 = create_checkpoint("s1", Some("first"), &sample_history(), None);
        cp1.created_at = "2025-01-01T00:00:00+00:00".to_string();
        store.save(&cp1).await.unwrap();

        let mut cp2 = create_checkpoint("s1", Some("second"), &sample_history(), None);
        cp2.created_at = "2025-01-02T00:00:00+00:00".to_string();
        store.save(&cp2).await.unwrap();

        let list = store.list("s1").await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].label.as_deref(), Some("second"));
        assert_eq!(list[1].label.as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn delete_checkpoint() {
        let store = InMemoryCheckpointStore::new();
        let cp = create_checkpoint("s1", None, &sample_history(), None);
        let id = store.save(&cp).await.unwrap();

        assert!(store.delete(&id).await.unwrap());
        assert!(store.load(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn clear_session_removes_all() {
        let store = InMemoryCheckpointStore::new();
        for i in 0..5 {
            let cp = create_checkpoint("s1", Some(&format!("cp-{i}")), &sample_history(), None);
            store.save(&cp).await.unwrap();
        }
        let cp_s2 = create_checkpoint("s2", Some("other"), &sample_history(), None);
        store.save(&cp_s2).await.unwrap();

        let removed = store.clear_session("s1").await.unwrap();
        assert_eq!(removed, 5);
        assert!(store.list("s1").await.unwrap().is_empty());
        assert_eq!(store.list("s2").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_false() {
        let store = InMemoryCheckpointStore::new();
        assert!(!store.delete("nonexistent").await.unwrap());
    }

    // ---- SQLite tests ----

    fn sqlite_store() -> SqliteCheckpointStore {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        SqliteCheckpointStore::new(Arc::new(Mutex::new(conn))).unwrap()
    }

    #[tokio::test]
    async fn sqlite_save_and_load() {
        let store = sqlite_store();
        let meta = serde_json::json!({"step": 3, "approved": true});
        let cp = create_checkpoint("s1", Some("label-a"), &sample_history(), Some(meta.clone()));
        let id = store.save(&cp).await.unwrap();

        let loaded = store
            .load(&id)
            .await
            .unwrap()
            .expect("checkpoint should exist");
        assert_eq!(loaded.session_id, "s1");
        assert_eq!(loaded.label.as_deref(), Some("label-a"));
        assert_eq!(loaded.history.len(), 2);
        assert_eq!(loaded.turn_count, 2);
        assert_eq!(loaded.metadata.unwrap(), meta);
    }

    #[tokio::test]
    async fn sqlite_list_ordered() {
        let store = sqlite_store();

        let mut cp1 = create_checkpoint("s1", Some("older"), &sample_history(), None);
        cp1.created_at = "2025-01-01T00:00:00+00:00".to_string();
        store.save(&cp1).await.unwrap();

        let mut cp2 = create_checkpoint("s1", Some("newer"), &sample_history(), None);
        cp2.created_at = "2025-01-02T00:00:00+00:00".to_string();
        store.save(&cp2).await.unwrap();

        let list = store.list("s1").await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].label.as_deref(), Some("newer"));
        assert_eq!(list[1].label.as_deref(), Some("older"));
    }

    #[tokio::test]
    async fn sqlite_delete_and_clear() {
        let store = sqlite_store();

        let cp1 = create_checkpoint("s1", Some("a"), &sample_history(), None);
        let cp2 = create_checkpoint("s1", Some("b"), &sample_history(), None);
        let cp3 = create_checkpoint("s1", Some("c"), &sample_history(), None);
        let id1 = store.save(&cp1).await.unwrap();
        store.save(&cp2).await.unwrap();
        store.save(&cp3).await.unwrap();

        // Delete one
        assert!(store.delete(&id1).await.unwrap());
        assert!(store.load(&id1).await.unwrap().is_none());
        assert_eq!(store.list("s1").await.unwrap().len(), 2);

        // Clear the rest
        let removed = store.clear_session("s1").await.unwrap();
        assert_eq!(removed, 2);
        assert!(store.list("s1").await.unwrap().is_empty());
    }
}
