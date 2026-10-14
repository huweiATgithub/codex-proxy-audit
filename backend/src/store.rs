//! Own the plugin's SQLite file, schema, transactions and stored projections.
//! Callers receive audit data and storage errors without database handles.

mod connection;

use std::{
    fmt::Write,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::Value;
use sqlx::{
    Connection, Row, Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use uuid::Uuid;

use crate::collection::exclusions::ExclusionSettings;
use crate::collection::{AuditDraft, ContentBlock};
use connection::{CLEANUP_WAIT, ConnectionLease, STATEMENT_WAIT, StatementBudget};

pub const RETENTION_BATCH_SIZE: i64 = 1_000;
const RETENTION_AGE: chrono::TimeDelta = chrono::TimeDelta::hours(72);
const DATA_DIRECTORY: &str = "huwei.prompt-audit";
const DATABASE_FILE: &str = "audit.sqlite3";

type StoreResult<T> = std::result::Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("audit storage is unavailable")]
    Unavailable(#[source] anyhow::Error),
    #[error("audit storage failed")]
    Internal(#[source] anyhow::Error),
}

impl StoreError {
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    fn internal(error: impl Into<anyhow::Error>) -> Self {
        Self::Internal(error.into())
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        let unavailable = match &error {
            sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => true,
            sqlx::Error::Database(error) => error
                .code()
                .and_then(|code| code.parse::<i32>().ok())
                .is_some_and(|code| matches!(code & 0xff, 5 | 6 | 7 | 8 | 9 | 10 | 13 | 14)),
            _ => false,
        };
        if unavailable {
            Self::Unavailable(error.into())
        } else {
            Self::Internal(error.into())
        }
    }
}

impl From<anyhow::Error> for StoreError {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<sqlx::Error>() {
            Ok(error) => error.into(),
            Err(error) => Self::Internal(error),
        }
    }
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

pub struct AuditListFilter<'a> {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub key: Option<&'a str>,
    pub search: &'a str,
    pub after: Option<(DateTime<Utc>, &'a str)>,
    pub limit: i64,
}

pub struct AuditList {
    pub total: i64,
    pub rows: Vec<AuditListRow>,
}

pub struct AuditListRow {
    pub id: String,
    pub key_id: String,
    pub first_observed_at: DateTime<Utc>,
    pub context_count: i64,
    pub excerpt: String,
    pub prefix: bool,
    pub suffix: bool,
}

pub struct AuditDetail {
    pub key_id: String,
    pub first_observed_at: DateTime<Utc>,
    pub request_id: Option<String>,
    pub thread_id: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub input_kinds: Value,
    pub repositories: Option<Value>,
    pub input_content_id: String,
    pub contents: Vec<ContentDescriptor>,
}

pub struct ContentDescriptor {
    pub id: String,
    pub role: Option<String>,
    pub phase: Option<String>,
    pub truncated: bool,
    pub character_count: i64,
    pub block_count: i64,
}

pub struct RequestReference {
    pub request_id: Option<String>,
    pub key_id: String,
}

/// Fixed-width code-point tokens retain literal substring search and bounded
/// SQL previews, including U+0000. ASCII folding leaves other letters alone.
pub fn encode_search_text(text: &str, fold_ascii: bool) -> String {
    let mut encoded = String::with_capacity(text.len().saturating_mul(7));
    for point in text.chars() {
        let point = if fold_ascii {
            point.to_ascii_lowercase()
        } else {
            point
        };
        let _ = write!(&mut encoded, "x{:06x}", point as u32);
    }
    encoded
}

pub fn decode_search_text(encoded: &str) -> Result<String> {
    if !encoded.len().is_multiple_of(7) || !encoded.is_ascii() {
        bail!("invalid stored audit text projection");
    }
    let mut text = String::new();
    for token in encoded.as_bytes().chunks_exact(7) {
        if token[0] != b'x' {
            bail!("invalid stored audit text token");
        }
        let value = u32::from_str_radix(std::str::from_utf8(&token[1..])?, 16)?;
        text.push(char::from_u32(value).context("invalid stored audit code point")?);
    }
    Ok(text)
}

pub fn encode_text(text: &str) -> String {
    encode_search_text(text, false)
}
pub fn search_text(text: &str) -> String {
    encode_search_text(text, true)
}
pub fn decode_text(encoded: &str) -> Result<String> {
    decode_search_text(encoded)
}

fn database_path(startup_directory: &Path) -> Result<PathBuf> {
    // Supported hosts start trusted plugins in <data>/plugins/plugin-*.
    // Keep the database outside that TempDir so package disposal cannot erase it.
    let cache = startup_directory
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "plugins"))
        .context("audit plugin startup directory is outside the host plugin cache")?;
    let data = cache
        .parent()
        .context("audit host data directory is missing")?;
    Ok(data.join(DATA_DIRECTORY).join(DATABASE_FILE))
}

// Stored times have millisecond precision. Ceil fractional query boundaries
// preserves strict comparisons against the original full-precision UTC bound.
fn boundary_millis(time: DateTime<Utc>) -> i64 {
    time.timestamp_millis() + i64::from(!time.timestamp_subsec_nanos().is_multiple_of(1_000_000))
}

fn stored_time(milliseconds: i64) -> StoreResult<DateTime<Utc>> {
    DateTime::from_timestamp_millis(milliseconds)
        .ok_or_else(|| StoreError::internal(anyhow::anyhow!("invalid stored audit timestamp")))
}

impl Store {
    pub async fn initialize() -> Result<Self> {
        Self::open(database_path(&std::env::current_dir()?)?).await
    }

    /// Opens a file owned by this plugin. Production selects its fixed path in
    /// initialize; tests use isolated temporary files through the same interface.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            let mut directory = fs::DirBuilder::new();
            directory.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                directory.mode(0o700);
            }
            directory
                .create(parent)
                .context("create audit data directory")?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(1));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(5))
            .idle_timeout(Duration::from_secs(60))
            .after_release(|connection, _| {
                Box::pin(async move {
                    // The worker finishes outstanding SQL/queued rollback before
                    // yielding this handle. Do not retain a cancelled deadline.
                    connection.lock_handle().await?.remove_progress_handler();
                    // SQLITE_INTERRUPT can automatically roll back a transaction
                    // while SQLx still tracks its old depth. Discard that handle.
                    Ok(!connection.is_in_transaction())
                })
            })
            .connect_with(options)
            .await
            .context("open audit SQLite")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .context("set audit database permissions")?;
        }
        let store = Self { pool };
        store.initialize_schema().await?;
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn lazy_memory() -> Self {
        Self {
            pool: SqlitePoolOptions::new().connect_lazy_with(SqliteConnectOptions::new()),
        }
    }

    async fn initialize_schema(&self) -> Result<()> {
        let mut lease = ConnectionLease::acquire(&self.pool, STATEMENT_WAIT).await?;
        let budget = &lease.budget;
        budget.start();
        let mut tx = lease.connection.begin_with("BEGIN IMMEDIATE").await?;
        budget.start();
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut *tx)
            .await?;
        match version {
            0 => {
                budget.start();
                sqlx::raw_sql(include_str!("store/schema.sql"))
                    .execute(&mut *tx)
                    .await?;
                budget.start();
                sqlx::query("PRAGMA user_version = 1")
                    .execute(&mut *tx)
                    .await?;
            }
            1 => {}
            _ => bail!("audit database schema is newer than this plugin"),
        }
        budget.start();
        tx.commit().await?;
        Ok(())
    }

    pub async fn time_zone(&self) -> StoreResult<Tz> {
        let mut lease = ConnectionLease::acquire(&self.pool, STATEMENT_WAIT).await?;
        lease.budget.start();
        let value: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT value FROM settings WHERE name = 'timeZone'")
                .fetch_optional(&mut *lease.connection)
                .await?;
        match value {
            None => Ok(chrono_tz::UTC),
            Some(value) => std::str::from_utf8(&value)
                .context("decode stored time zone")?
                .parse::<Tz>()
                .map_err(StoreError::internal),
        }
    }

    pub async fn save_time_zone(&self, time_zone: Tz) -> StoreResult<()> {
        let mut lease = ConnectionLease::acquire(&self.pool, STATEMENT_WAIT).await?;
        lease.budget.start();
        sqlx::query("INSERT INTO settings (name, value) VALUES ('timeZone', ?1) ON CONFLICT (name) DO UPDATE SET value = excluded.value")
            .bind(time_zone.name().as_bytes()).execute(&mut *lease.connection).await?;
        Ok(())
    }

    pub(crate) async fn exclusion_settings(&self) -> StoreResult<Option<ExclusionSettings>> {
        let mut lease = ConnectionLease::acquire(&self.pool, STATEMENT_WAIT).await?;
        lease.budget.start();
        let value: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT value FROM settings WHERE name = 'exclusions'")
                .fetch_optional(&mut *lease.connection)
                .await?;
        value
            .map(|value| serde_json::from_slice(&value).map_err(StoreError::internal))
            .transpose()
    }

    pub(crate) async fn save_exclusion_settings(
        &self,
        settings: &ExclusionSettings,
    ) -> StoreResult<()> {
        let value = serde_json::to_vec(settings).map_err(StoreError::internal)?;
        let mut lease = ConnectionLease::acquire(&self.pool, STATEMENT_WAIT).await?;
        lease.budget.start();
        sqlx::query("INSERT INTO settings (name, value) VALUES ('exclusions', ?1) ON CONFLICT (name) DO UPDATE SET value = excluded.value")
            .bind(value)
            .execute(&mut *lease.connection)
            .await?;
        Ok(())
    }

    pub async fn secret(&self, name: &str) -> StoreResult<Vec<u8>> {
        let mut bytes = Uuid::new_v4().as_bytes().to_vec();
        bytes.extend_from_slice(Uuid::new_v4().as_bytes());
        let mut lease = ConnectionLease::acquire(&self.pool, STATEMENT_WAIT).await?;
        lease.budget.start();
        sqlx::query(
            "INSERT INTO settings (name, value) VALUES (?1, ?2) ON CONFLICT (name) DO NOTHING",
        )
        .bind(name)
        .bind(bytes)
        .execute(&mut *lease.connection)
        .await?;
        lease.budget.start();
        Ok(
            sqlx::query_scalar("SELECT value FROM settings WHERE name = ?1")
                .bind(name)
                .fetch_one(&mut *lease.connection)
                .await?,
        )
    }

    pub async fn persist_batch(&self, drafts: &[AuditDraft]) -> StoreResult<()> {
        if drafts.is_empty() {
            return Ok(());
        }
        let mut lease = ConnectionLease::acquire(&self.pool, STATEMENT_WAIT).await?;
        let budget = &lease.budget;
        budget.start();
        let mut tx = lease.connection.begin_with("BEGIN IMMEDIATE").await?;
        for draft in drafts {
            insert_audit(&mut tx, budget, draft).await?;
        }
        budget.start();
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_expired_batch(&self, now: DateTime<Utc>) -> StoreResult<u64> {
        let mut lease = ConnectionLease::acquire(&self.pool, CLEANUP_WAIT).await?;
        let budget = &lease.budget;
        budget.start();
        let mut tx = lease.connection.begin_with("BEGIN IMMEDIATE").await?;
        budget.start();
        let result = sqlx::query("DELETE FROM audits WHERE id IN (SELECT id FROM audits WHERE first_observed_at < ?1 ORDER BY first_observed_at, id LIMIT ?2)")
            .bind(boundary_millis(now - RETENTION_AGE)).bind(RETENTION_BATCH_SIZE)
            .execute(&mut *tx).await?;
        budget.start();
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    pub async fn list(&self, filter: AuditListFilter<'_>) -> StoreResult<AuditList> {
        let start = filter.start.map(boundary_millis);
        let end = filter.end.map(boundary_millis);
        let folded = search_text(filter.search);
        let after_time = filter.after.map(|(time, _)| boundary_millis(time));
        let after_id = filter.after.map(|(_, id)| id);
        let after_exact = filter
            .after
            .is_none_or(|(time, _)| time.timestamp_subsec_nanos().is_multiple_of(1_000_000));
        let mut lease = ConnectionLease::acquire(&self.pool, STATEMENT_WAIT).await?;
        let budget = &lease.budget;
        budget.start();
        let mut tx = lease.connection.begin().await?;
        budget.start();
        let total = sqlx::query_scalar("SELECT count(*) FROM audits WHERE (?1 IS NULL OR first_observed_at >= ?1) AND (?2 IS NULL OR first_observed_at < ?2) AND (?3 IS NULL OR key_id = ?3) AND instr(input_search, ?4) > 0")
            .bind(start).bind(end).bind(filter.key).bind(&folded).fetch_one(&mut *tx).await?;
        budget.start();
        let rows = sqlx::query("SELECT id,key_id,first_observed_at,context_count, CASE WHEN ?4='' THEN preview ELSE substr(input_text,max(instr(input_search,?4)-245,1),2786) END AS excerpt, CASE WHEN ?4='' THEN false ELSE instr(input_search,?4)>246 END AS prefix, CASE WHEN ?4='' THEN false ELSE length(input_text)>max(instr(input_search,?4)-245,1)+2785 END AS suffix FROM audits WHERE (?1 IS NULL OR first_observed_at >= ?1) AND (?2 IS NULL OR first_observed_at < ?2) AND (?3 IS NULL OR key_id=?3) AND instr(input_search,?4)>0 AND (?5 IS NULL OR first_observed_at < ?5 OR (?8 AND first_observed_at=?5 AND id COLLATE BINARY < ?6)) ORDER BY first_observed_at DESC,id COLLATE BINARY DESC LIMIT ?7")
            .bind(start).bind(end).bind(filter.key).bind(&folded).bind(after_time).bind(after_id).bind(filter.limit + 1).bind(after_exact)
            .fetch_all(&mut *tx).await?;
        budget.start();
        tx.commit().await?;
        let rows = rows
            .into_iter()
            .map(|row| {
                Ok(AuditListRow {
                    id: row.try_get("id")?,
                    key_id: row.try_get("key_id")?,
                    first_observed_at: stored_time(row.try_get("first_observed_at")?)?,
                    context_count: row.try_get("context_count")?,
                    excerpt: decode_text(&row.try_get::<String, _>("excerpt")?)?,
                    prefix: row.try_get("prefix")?,
                    suffix: row.try_get("suffix")?,
                })
            })
            .collect::<StoreResult<Vec<_>>>()?;
        Ok(AuditList { total, rows })
    }

    pub async fn detail(&self, id: &str) -> StoreResult<Option<AuditDetail>> {
        let mut lease = ConnectionLease::acquire(&self.pool, STATEMENT_WAIT).await?;
        let budget = &lease.budget;
        budget.start();
        let mut tx = lease.connection.begin().await?;
        budget.start();
        let audit = sqlx::query("SELECT key_id,first_observed_at,request_id,thread_id,session_id,turn_id,input_kinds,repositories,input_content_id FROM audits WHERE id=?1")
            .bind(id).fetch_optional(&mut *tx).await?;
        let Some(audit) = audit else {
            return Ok(None);
        };
        budget.start();
        let rows = sqlx::query("SELECT id,role,phase,truncated,character_count,block_count FROM contents WHERE audit_id=?1 ORDER BY ordinal")
            .bind(id).fetch_all(&mut *tx).await?;
        budget.start();
        tx.commit().await?;
        let contents = rows
            .into_iter()
            .map(|row| {
                Ok(ContentDescriptor {
                    id: row.try_get("id")?,
                    role: row.try_get("role")?,
                    phase: row.try_get("phase")?,
                    truncated: row.try_get("truncated")?,
                    character_count: row.try_get("character_count")?,
                    block_count: row.try_get("block_count")?,
                })
            })
            .collect::<StoreResult<Vec<_>>>()?;
        Ok(Some(AuditDetail {
            key_id: audit.try_get("key_id")?,
            first_observed_at: stored_time(audit.try_get("first_observed_at")?)?,
            request_id: audit.try_get("request_id")?,
            thread_id: audit.try_get("thread_id")?,
            session_id: audit.try_get("session_id")?,
            turn_id: audit.try_get("turn_id")?,
            input_kinds: audit.try_get("input_kinds")?,
            repositories: audit.try_get("repositories")?,
            input_content_id: audit.try_get("input_content_id")?,
            contents,
        }))
    }

    pub async fn content(&self, audit_id: &str, content_id: &str) -> StoreResult<Option<Value>> {
        let mut lease = ConnectionLease::acquire(&self.pool, STATEMENT_WAIT).await?;
        lease.budget.start();
        Ok(
            sqlx::query_scalar("SELECT blocks FROM contents WHERE audit_id=?1 AND id=?2")
                .bind(audit_id)
                .bind(content_id)
                .fetch_optional(&mut *lease.connection)
                .await?,
        )
    }

    pub async fn request_reference(&self, id: &str) -> StoreResult<Option<RequestReference>> {
        let mut lease = ConnectionLease::acquire(&self.pool, STATEMENT_WAIT).await?;
        lease.budget.start();
        sqlx::query("SELECT request_id,key_id FROM audits WHERE id=?1")
            .bind(id)
            .fetch_optional(&mut *lease.connection)
            .await?
            .map(|row| {
                Ok(RequestReference {
                    request_id: row.try_get("request_id")?,
                    key_id: row.try_get("key_id")?,
                })
            })
            .transpose()
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

async fn insert_audit(
    tx: &mut Transaction<'_, Sqlite>,
    budget: &StatementBudget,
    draft: &AuditDraft,
) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    let input_content_id = Uuid::new_v4().to_string();
    let input_text = draft
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Nontext { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let preview_source = draft
        .blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::Nontext { media_type, .. } => format!("[{media_type}：非文本内容未采集]"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut preview = preview_source.chars().take(398).collect::<String>();
    if preview_source.chars().count() > 398 {
        preview.push('…');
    }
    let query = sqlx::query(
        "INSERT INTO audits \
         (id, key_id, first_observed_at, \
          request_id, thread_id, session_id, turn_id, input_kinds, repositories, \
          input_content_id, input_text, input_search, preview, context_count) \
         VALUES (?1,?2,?3, \
                 ?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
    )
    .bind(&id)
    .bind(&draft.key_id)
    .bind(draft.observed_at.timestamp_millis())
    .bind(&draft.request_id)
    .bind(&draft.thread_id)
    .bind(&draft.session_id)
    .bind(&draft.turn_id)
    .bind(serde_json::to_string(&draft.input_kinds)?)
    .bind(
        draft
            .repositories
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
    )
    .bind(&input_content_id)
    .bind(encode_search_text(&input_text, false))
    .bind(encode_search_text(&input_text, true))
    .bind(encode_text(&preview))
    .bind(i64::try_from(draft.context.len())?);
    budget.start();
    query.execute(&mut **tx).await?;
    insert_content(
        tx,
        budget,
        &id,
        &input_content_id,
        -1,
        None,
        None,
        false,
        &draft.blocks,
    )
    .await?;
    for (ordinal, context) in draft.context.iter().enumerate() {
        insert_content(
            tx,
            budget,
            &id,
            &Uuid::new_v4().to_string(),
            i32::try_from(ordinal)?,
            Some(&context.role),
            context.phase.as_deref(),
            context.truncated,
            &context.blocks,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_content(
    tx: &mut Transaction<'_, Sqlite>,
    budget: &StatementBudget,
    audit_id: &str,
    id: &str,
    ordinal: i32,
    role: Option<&str>,
    phase: Option<&str>,
    truncated: bool,
    blocks: &[ContentBlock],
) -> Result<()> {
    let characters = blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.chars().count(),
            ContentBlock::Nontext { .. } => 0,
        })
        .sum::<usize>();
    let query = sqlx::query(
        "INSERT INTO contents \
         (id,audit_id,ordinal,role,phase,truncated,blocks,character_count,block_count) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
    )
    .bind(id)
    .bind(audit_id)
    .bind(ordinal)
    .bind(role)
    .bind(phase)
    .bind(truncated)
    .bind(serde_json::to_string(blocks)?)
    .bind(i64::try_from(characters)?)
    .bind(i64::try_from(blocks.len())?);
    budget.start();
    query.execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn package_reextraction_preserves_private_database() -> Result<()> {
        let data = tempfile::tempdir()?;
        let cache = data.path().join("plugins");
        fs::create_dir(&cache)?;
        let package = tempfile::Builder::new()
            .prefix("plugin-")
            .tempdir_in(&cache)?;
        let path = database_path(package.path())?;
        let store = Store::open(&path).await?;
        let secret = store.secret("query_cursor").await?;
        store.save_time_zone(chrono_tz::Asia::Tokyo).await?;
        store.close().await;
        drop(package);
        assert!(path.is_file());

        let package = tempfile::Builder::new()
            .prefix("plugin-")
            .tempdir_in(&cache)?;
        assert_eq!(database_path(package.path())?, path);
        let store = Store::open(path).await?;
        assert_eq!(store.secret("query_cursor").await?, secret);
        assert_eq!(store.time_zone().await?, chrono_tz::Asia::Tokyo);
        store.close().await;
        Ok(())
    }

    #[test]
    fn startup_directory_outside_host_cache_is_rejected() {
        assert!(database_path(Path::new("/host/data")).is_err());
    }
}
