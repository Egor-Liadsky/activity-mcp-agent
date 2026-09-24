//! Хранилище демона: SQLite через `sqlx`. Модуль ничего не знает ни про git,
//! ни про MCP — только строки таблиц из `migrations/`.

use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

pub type Result<T> = std::result::Result<T, sqlx::Error>;

/// Проект, как он лежит в таблице `projects`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRow {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub head_branch: Option<String>,
    pub dirty_files: i64,
    pub dirty_sample: Vec<String>,
    pub first_seen: i64,
    pub last_scanned: Option<i64>,
    pub last_activity: Option<i64>,
    pub removed_at: Option<i64>,
}

/// Событие журнала вместе с именем проекта — так его читают сводка и
/// инструменты.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    pub id: i64,
    pub project_id: i64,
    pub project: String,
    pub kind: String,
    pub seen_at: i64,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DigestRow {
    pub id: i64,
    pub period_from: i64,
    pub period_to: i64,
    pub created_at: i64,
    pub body: Value,
    pub text: String,
    pub acked_at: Option<i64>,
}

/// Снимок состояния рабочего дерева после опроса.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub head_branch: Option<String>,
    pub dirty_files: i64,
    pub dirty_sample: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Открывает (создаёт) файл базы и применяет миграции.
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|err| anyhow::anyhow!("не удалось создать каталог базы {}: {err}", parent.display()))?;
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite:{}", path.display()))?
            .create_if_missing(true)
            // WAL: демон пишет, а `--stdio`-экземпляр для другого клиента
            // в это время читает ту же базу без блокировки.
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        Self::connect(options, 4).await
    }

    /// База в памяти для тестов. Одно соединение: у каждого соединения
    /// `:memory:` была бы своя пустая база.
    pub async fn open_in_memory() -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        Self::connect(options, 1).await
    }

    async fn connect(options: SqliteConnectOptions, max_connections: u32) -> anyhow::Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }

    // --- проекты ---------------------------------------------------------

    /// Регистрирует проект или возвращает уже известный. Второй элемент —
    /// признак того, что проект новый (или вернулся после исчезновения).
    pub async fn upsert_project(&self, path: &str, name: &str, now: i64) -> Result<(i64, bool)> {
        if let Some(row) = sqlx::query("SELECT id, removed_at FROM projects WHERE path = ?")
            .bind(path)
            .fetch_optional(&self.pool)
            .await?
        {
            let id: i64 = row.get("id");
            let removed: Option<i64> = row.get("removed_at");
            if removed.is_some() {
                sqlx::query("UPDATE projects SET removed_at = NULL, name = ? WHERE id = ?")
                    .bind(name)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
            return Ok((id, removed.is_some()));
        }
        let id = sqlx::query("INSERT INTO projects (path, name, first_seen) VALUES (?, ?, ?)")
            .bind(path)
            .bind(name)
            .bind(now)
            .execute(&self.pool)
            .await?
            .last_insert_rowid();
        Ok((id, true))
    }

    pub async fn mark_removed(&self, id: i64, now: i64) -> Result<()> {
        sqlx::query("UPDATE projects SET removed_at = ? WHERE id = ? AND removed_at IS NULL")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Все проекты, включая исчезнувшие, по имени.
    pub async fn projects(&self) -> Result<Vec<ProjectRow>> {
        let rows = sqlx::query("SELECT * FROM projects ORDER BY name")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(project_from_row).collect())
    }

    pub async fn active_projects(&self) -> Result<Vec<ProjectRow>> {
        Ok(self
            .projects()
            .await?
            .into_iter()
            .filter(|project| project.removed_at.is_none())
            .collect())
    }

    /// Проект по имени или по абсолютному пути.
    pub async fn find_project(&self, name_or_path: &str) -> Result<Option<ProjectRow>> {
        let row = sqlx::query("SELECT * FROM projects WHERE name = ?1 OR path = ?1 ORDER BY removed_at IS NOT NULL LIMIT 1")
            .bind(name_or_path)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(project_from_row))
    }

    pub async fn save_snapshot(&self, id: i64, snapshot: &Snapshot, now: i64) -> Result<()> {
        sqlx::query(
            "UPDATE projects SET head_branch = ?, dirty_files = ?, dirty_sample = ?, last_scanned = ? WHERE id = ?",
        )
        .bind(&snapshot.head_branch)
        .bind(snapshot.dirty_files)
        .bind(serde_json::to_string(&snapshot.dirty_sample).unwrap_or_else(|_| "[]".into()))
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // --- курсоры веток ---------------------------------------------------

    pub async fn refs(&self, project_id: i64) -> Result<HashMap<String, String>> {
        let rows = sqlx::query("SELECT name, sha FROM refs WHERE project_id = ?")
            .bind(project_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|row| (row.get("name"), row.get("sha"))).collect())
    }

    pub async fn set_ref(&self, project_id: i64, name: &str, sha: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO refs (project_id, name, sha) VALUES (?, ?, ?)
             ON CONFLICT (project_id, name) DO UPDATE SET sha = excluded.sha",
        )
        .bind(project_id)
        .bind(name)
        .bind(sha)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_ref(&self, project_id: i64, name: &str) -> Result<()> {
        sqlx::query("DELETE FROM refs WHERE project_id = ? AND name = ?")
            .bind(project_id)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- события ---------------------------------------------------------

    /// Записывает событие. `false` — коммит уже был записан раньше (из
    /// другой ветки), повторно он не учитывается.
    pub async fn insert_event(
        &self,
        project_id: i64,
        kind: &str,
        seen_at: i64,
        sha: Option<&str>,
        payload: &Value,
    ) -> Result<bool> {
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO events (project_id, kind, seen_at, sha, payload) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(project_id)
        .bind(kind)
        .bind(seen_at)
        .bind(sha)
        .bind(payload.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0;
        if inserted {
            sqlx::query("UPDATE projects SET last_activity = MAX(COALESCE(last_activity, 0), ?) WHERE id = ?")
                .bind(seen_at)
                .bind(project_id)
                .execute(&self.pool)
                .await?;
        }
        Ok(inserted)
    }

    /// События периода `(from, to]` в порядке появления.
    pub async fn events_between(&self, from: i64, to: i64) -> Result<Vec<EventRow>> {
        let rows = sqlx::query(
            "SELECT e.id, e.project_id, p.name AS project, e.kind, e.seen_at, e.payload
             FROM events e JOIN projects p ON p.id = e.project_id
             WHERE e.seen_at > ? AND e.seen_at <= ?
             ORDER BY e.seen_at, e.id",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(event_from_row).collect())
    }

    /// Последние события, новые первыми; по проекту или по всем.
    pub async fn recent_events(&self, project_id: Option<i64>, since: i64, limit: i64) -> Result<Vec<EventRow>> {
        let rows = sqlx::query(
            "SELECT e.id, e.project_id, p.name AS project, e.kind, e.seen_at, e.payload
             FROM events e JOIN projects p ON p.id = e.project_id
             WHERE e.seen_at >= ?1 AND (?2 IS NULL OR e.project_id = ?2)
             ORDER BY e.seen_at DESC, e.id DESC
             LIMIT ?3",
        )
        .bind(since)
        .bind(project_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(event_from_row).collect())
    }

    /// Время самого раннего события — начало первой сводки.
    pub async fn earliest_event(&self) -> Result<Option<i64>> {
        sqlx::query_scalar("SELECT MIN(seen_at) FROM events")
            .fetch_one(&self.pool)
            .await
    }

    // --- сводки ----------------------------------------------------------

    pub async fn insert_digest(
        &self,
        period_from: i64,
        period_to: i64,
        created_at: i64,
        body: &Value,
        text: &str,
    ) -> Result<i64> {
        Ok(sqlx::query(
            "INSERT INTO digests (period_from, period_to, created_at, body, text) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(period_from)
        .bind(period_to)
        .bind(created_at)
        .bind(body.to_string())
        .bind(text)
        .execute(&self.pool)
        .await?
        .last_insert_rowid())
    }

    /// Конец периода последней сводки — начало следующей.
    pub async fn last_period_to(&self) -> Result<Option<i64>> {
        sqlx::query_scalar("SELECT MAX(period_to) FROM digests")
            .fetch_one(&self.pool)
            .await
    }

    /// Сводки, новые первыми.
    pub async fn digests(&self, unread_only: bool, limit: i64) -> Result<Vec<DigestRow>> {
        let rows = sqlx::query(
            "SELECT * FROM digests WHERE (?1 = 0 OR acked_at IS NULL) ORDER BY period_to DESC, id DESC LIMIT ?2",
        )
        .bind(unread_only)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(digest_from_row).collect())
    }

    /// Помечает сводку прочитанной. `false` — такой сводки нет.
    pub async fn ack_digest(&self, id: i64, now: i64) -> Result<bool> {
        let exists = sqlx::query("UPDATE digests SET acked_at = COALESCE(acked_at, ?) WHERE id = ?")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0;
        Ok(exists)
    }

    /// Удаляет события и сводки старше `before`. Курсоры не трогает: иначе
    /// следующий опрос принял бы старую историю за новые коммиты.
    pub async fn prune(&self, before: i64) -> Result<u64> {
        let events = sqlx::query("DELETE FROM events WHERE seen_at < ?")
            .bind(before)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let digests = sqlx::query("DELETE FROM digests WHERE period_to < ?")
            .bind(before)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(events + digests)
    }
}

fn project_from_row(row: &sqlx::sqlite::SqliteRow) -> ProjectRow {
    let sample: String = row.get("dirty_sample");
    ProjectRow {
        id: row.get("id"),
        path: row.get("path"),
        name: row.get("name"),
        head_branch: row.get("head_branch"),
        dirty_files: row.get("dirty_files"),
        dirty_sample: serde_json::from_str(&sample).unwrap_or_default(),
        first_seen: row.get("first_seen"),
        last_scanned: row.get("last_scanned"),
        last_activity: row.get("last_activity"),
        removed_at: row.get("removed_at"),
    }
}

fn event_from_row(row: &sqlx::sqlite::SqliteRow) -> EventRow {
    let payload: String = row.get("payload");
    EventRow {
        id: row.get("id"),
        project_id: row.get("project_id"),
        project: row.get("project"),
        kind: row.get("kind"),
        seen_at: row.get("seen_at"),
        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
    }
}

fn digest_from_row(row: &sqlx::sqlite::SqliteRow) -> DigestRow {
    let body: String = row.get("body");
    DigestRow {
        id: row.get("id"),
        period_from: row.get("period_from"),
        period_to: row.get("period_to"),
        created_at: row.get("created_at"),
        body: serde_json::from_str(&body).unwrap_or(Value::Null),
        text: row.get("text"),
        acked_at: row.get("acked_at"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn project_returns_after_removal_as_new() {
        let store = Store::open_in_memory().await.unwrap();
        let (id, created) = store.upsert_project("/r/a", "a", 10).await.unwrap();
        assert!(created);
        assert_eq!(store.upsert_project("/r/a", "a", 11).await.unwrap(), (id, false));
        store.mark_removed(id, 12).await.unwrap();
        assert!(store.active_projects().await.unwrap().is_empty());
        assert_eq!(store.upsert_project("/r/a", "a", 13).await.unwrap(), (id, true));
        assert_eq!(store.find_project("/r/a").await.unwrap().unwrap().name, "a");
        assert_eq!(store.find_project("a").await.unwrap().unwrap().id, id);
    }

    #[tokio::test]
    async fn commit_is_recorded_once_per_project() {
        let store = Store::open_in_memory().await.unwrap();
        let (id, _) = store.upsert_project("/r/a", "a", 1).await.unwrap();
        let payload = json!({ "subject": "x" });
        assert!(store.insert_event(id, "commit", 5, Some("abc"), &payload).await.unwrap());
        assert!(!store.insert_event(id, "commit", 6, Some("abc"), &payload).await.unwrap());
        // События без sha уникальностью не ограничены.
        assert!(store.insert_event(id, "checkout", 7, None, &payload).await.unwrap());
        assert!(store.insert_event(id, "checkout", 8, None, &payload).await.unwrap());
        assert_eq!(store.events_between(0, 100).await.unwrap().len(), 3);
        assert_eq!(store.events_between(5, 7).await.unwrap().len(), 1);
        assert_eq!(store.earliest_event().await.unwrap(), Some(5));
        assert_eq!(store.projects().await.unwrap()[0].last_activity, Some(8));
    }

    #[tokio::test]
    async fn digests_are_read_and_acked() {
        let store = Store::open_in_memory().await.unwrap();
        assert_eq!(store.last_period_to().await.unwrap(), None);
        let first = store.insert_digest(0, 10, 10, &json!({}), "один").await.unwrap();
        let second = store.insert_digest(10, 20, 20, &json!({}), "два").await.unwrap();
        assert_eq!(store.last_period_to().await.unwrap(), Some(20));
        let unread = store.digests(true, 10).await.unwrap();
        assert_eq!(unread.iter().map(|d| d.id).collect::<Vec<_>>(), vec![second, first]);
        assert!(store.ack_digest(second, 30).await.unwrap());
        assert!(!store.ack_digest(999, 30).await.unwrap());
        assert_eq!(store.digests(true, 10).await.unwrap().len(), 1);
        assert_eq!(store.digests(false, 10).await.unwrap().len(), 2);
        assert_eq!(store.prune(15).await.unwrap(), 1);
    }
}
