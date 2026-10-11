//! 管理审计数据库迁移、批量写入和查询所需的元数据
//! 数据库凭据由插件握手提供，用户输入与上下文在同一事务中保存

use std::fmt::Write;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgPoolOptions};
use uuid::Uuid;

use crate::collection::{AuditDraft, ContentBlock};

pub const RETENTION_BATCH_SIZE: i64 = 1_000;
const RETENTION_AGE: chrono::TimeDelta = chrono::TimeDelta::hours(72);

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostRequestMetadata {
    pub user_agent: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

/// Fixed-width code-point tokens preserve U+0000 (which PostgreSQL TEXT cannot
/// store) while retaining literal substring search and bounded SQL previews.
/// `x` cannot occur inside a hexadecimal payload, so matches cannot begin at a
/// partial code point. ASCII folding deliberately leaves all other letters alone.
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

impl Store {
    pub async fn connect(database_url: &str) -> Result<Self> {
        // SQLx errors can contain endpoint information, so callers log their
        // operation/context rather than formatting the supplied credential.
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(5))
            .idle_timeout(Duration::from_secs(60))
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET statement_timeout = '1500ms'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET lock_timeout = '1000ms'")
                        .execute(&mut *connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(database_url)
            .await
            .context("connect audit PostgreSQL")?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// One read per query; an absent shared setting means UTC.
    pub async fn time_zone(&self) -> Result<Tz, sqlx::Error> {
        let value: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT value FROM prompt_audit.settings WHERE name = 'timeZone'")
                .fetch_optional(&self.pool)
                .await?;
        match value {
            None => Ok(chrono_tz::UTC),
            Some(value) => std::str::from_utf8(&value)
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?
                .parse::<Tz>()
                .map_err(|error| sqlx::Error::Decode(Box::new(error))),
        }
    }

    pub async fn save_time_zone(&self, time_zone: Tz) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO prompt_audit.settings (name, value) VALUES ('timeZone', $1) \
             ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(time_zone.name().as_bytes())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn migrate(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // A fixed application lock protects first installation and upgrades
        // across plugin processes without touching the host migration table.
        sqlx::query("SELECT pg_advisory_xact_lock(728019240991)")
            .execute(&mut *tx)
            .await?;
        let schema_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT FROM pg_namespace WHERE nspname = 'prompt_audit')",
        )
        .fetch_one(&mut *tx)
        .await?;
        if !schema_exists {
            bail!("prompt_audit schema is not provisioned; run scripts/provision.sql first");
        }
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS prompt_audit.schema_migrations \
             (version BIGINT PRIMARY KEY, applied_at TIMESTAMPTZ NOT NULL DEFAULT now())",
        )
        .execute(&mut *tx)
        .await?;
        let version: i64 = sqlx::query_scalar(
            "SELECT coalesce(max(version), 0) FROM prompt_audit.schema_migrations",
        )
        .fetch_one(&mut *tx)
        .await?;
        if version > 3 {
            bail!("audit database schema is newer than this plugin");
        }
        for (target, migration) in [
            (1, include_str!("../migrations/0001_audit.sql")),
            (2, include_str!("../migrations/0002_request_audit.sql")),
            (3, include_str!("../migrations/0003_live_key_names.sql")),
        ] {
            if version < target {
                sqlx::raw_sql(migration).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO prompt_audit.schema_migrations (version) VALUES ($1)")
                    .bind(target)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Persist a random cursor signing secret once, retaining cursors on restart.
    pub async fn secret(&self, name: &str) -> Result<Vec<u8>> {
        let mut bytes = Uuid::new_v4().as_bytes().to_vec();
        bytes.extend_from_slice(Uuid::new_v4().as_bytes());
        sqlx::query(
            "INSERT INTO prompt_audit.settings (name, value) VALUES ($1, $2) \
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(name)
        .bind(bytes)
        .execute(&self.pool)
        .await?;
        Ok(
            sqlx::query_scalar("SELECT value FROM prompt_audit.settings WHERE name = $1")
                .bind(name)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// 同一批次的审计记录与正文原子写入，重复任务各自生成记录
    pub async fn persist_batch(&self, drafts: &[AuditDraft]) -> Result<()> {
        if drafts.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for draft in drafts {
            insert_audit(&mut tx, draft).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 删除超过固定 72 小时保留期的最旧记录，正文通过外键在同一事务级联删除。
    /// 每批独立提交；跳过其他实例或操作持有的行锁，下一轮再处理。
    pub async fn delete_expired_batch(&self, now: DateTime<Utc>) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        // Keep the longer cleanup timeout local to this transaction so reads
        // and writes retain their normal timeout when the connection is reused.
        sqlx::query("SET LOCAL statement_timeout = '3s'")
            .execute(&mut *tx)
            .await?;
        let result = sqlx::query(
            "WITH expired AS ( \
                 SELECT id FROM prompt_audit.audits \
                 WHERE first_observed_at < $1 \
                 ORDER BY first_observed_at, id \
                 FOR UPDATE SKIP LOCKED LIMIT $2 \
             ) \
             DELETE FROM prompt_audit.audits AS audit USING expired \
             WHERE audit.id = expired.id",
        )
        .bind(now - RETENTION_AGE)
        .bind(RETENTION_BATCH_SIZE)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    pub async fn request_metadata(
        &self,
        request_id: Option<&str>,
        key_id: &str,
    ) -> Result<Option<HostRequestMetadata>> {
        let Some(request_id) = request_id else {
            return Ok(None);
        };
        let row = sqlx::query(
            "SELECT user_agent, requested_model_id, reasoning_effort \
             FROM public.model_request_observations \
             WHERE id::text = $1 AND client_api_key_ref::text = $2",
        )
        .bind(request_id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(HostRequestMetadata {
                user_agent: row.try_get("user_agent")?,
                model: row.try_get("requested_model_id")?,
                reasoning_effort: row.try_get("reasoning_effort")?,
            })
        })
        .transpose()
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

async fn insert_audit(tx: &mut Transaction<'_, Postgres>, draft: &AuditDraft) -> Result<()> {
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
    sqlx::query(
        "INSERT INTO prompt_audit.audits \
         (id, key_id, first_observed_at, \
          request_id, thread_id, session_id, turn_id, input_kinds, repositories, \
          input_content_id, input_text, input_search, preview, context_count) \
         VALUES ($1,$2,date_trunc('milliseconds',$3::timestamptz), \
                 $4,$5,$6,$7,$8::json,$9::json,$10,$11,$12,$13,$14)",
    )
    .bind(&id)
    .bind(&draft.key_id)
    .bind(draft.observed_at)
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
    .bind(i64::try_from(draft.context.len())?)
    .execute(&mut **tx)
    .await?;
    insert_content(
        tx,
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
    tx: &mut Transaction<'_, Postgres>,
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
    sqlx::query(
        "INSERT INTO prompt_audit.contents \
         (id,audit_id,ordinal,role,phase,truncated,blocks,character_count,block_count) \
         VALUES ($1,$2,$3,$4,$5,$6,$7::json,$8,$9)",
    )
    .bind(id)
    .bind(audit_id)
    .bind(ordinal)
    .bind(role)
    .bind(phase)
    .bind(truncated)
    .bind(serde_json::to_string(blocks)?)
    .bind(i64::try_from(characters)?)
    .bind(i64::try_from(blocks.len())?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
