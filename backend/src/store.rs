//! PostgreSQL ownership and transactions for immutable audit facts.
//!
//! The credential is supplied by the plugin handshake. This module never reads
//! DATABASE_URL, host Key tables, or host environment variables.

use std::fmt::Write;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgPoolOptions};
use uuid::Uuid;

use crate::collection::{
    AuditDraft, ContentBlock, RequestObservation, ResponseContext, ResponseSnapshot, SessionState,
    apply_response, collect, scope,
};

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

#[derive(Debug, Clone)]
pub struct ObservedKey {
    pub id: String,
    pub name: Option<String>,
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
        match version {
            0 => {
                sqlx::raw_sql(include_str!("../migrations/0001_audit.sql"))
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO prompt_audit.schema_migrations (version) VALUES (1)")
                    .execute(&mut *tx)
                    .await?;
            }
            1 => {}
            _ => bail!("audit database schema is newer than this plugin"),
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

    /// Identify inputs while holding the database scope lock. Facts, bodies,
    /// and the state used to identify the next request commit together.
    pub async fn collect_request(
        &self,
        observation: &RequestObservation,
    ) -> Result<crate::collection::PreparedCollection> {
        let identity_scope = scope(observation);
        let mut tx = self.pool.begin().await?;
        lock_scope(&mut tx, &identity_scope).await?;
        let state = load_state(&mut tx, &identity_scope).await?;
        let prepared = collect(&state, observation);
        for draft in &prepared.audits {
            insert_audit(&mut tx, &identity_scope, observation, draft).await?;
        }
        save_state(&mut tx, &identity_scope, &prepared.state).await?;
        tx.commit().await?;
        Ok(prepared)
    }

    /// Response observations only update the bounded rolling state. Audit
    /// bodies and input-time context snapshots are never updated here.
    pub async fn persist_response(
        &self,
        context: &ResponseContext,
        snapshot: &ResponseSnapshot,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        lock_scope(&mut tx, &context.scope).await?;
        let mut state = load_state(&mut tx, &context.scope).await?;
        apply_response(&mut state, context, snapshot);
        save_state(&mut tx, &context.scope, &state).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Supply only a successfully fetched complete host.keys.list directory.
    /// Failed/partial pagination must leave the old membership intact.
    pub async fn update_keys(
        &self,
        keys: &[ObservedKey],
        observed_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(728019240992)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE prompt_audit.key_directory SET present = false")
            .execute(&mut *tx)
            .await?;
        for key in keys {
            let name = key
                .name
                .as_deref()
                .filter(|name| !name.contains('\0'))
                .map(|name| name.chars().take(512).collect::<String>());
            save_key(&mut tx, &key.id, name.as_deref(), observed_at, true).await?;
        }
        tx.commit().await?;
        Ok(())
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

    /// Rolling state is an optimization with explicit lifetime/count bounds;
    /// dropping it does not delete the independent audit facts or identities.
    pub async fn prune_sessions(&self) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM prompt_audit.sessions \
             WHERE updated_at < now() - interval '7 days'",
        )
        .execute(&self.pool)
        .await?;
        let overflow = sqlx::query(
            "DELETE FROM prompt_audit.sessions WHERE scope IN \
             (SELECT scope FROM prompt_audit.sessions \
              ORDER BY updated_at DESC, scope OFFSET 10000)",
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() + overflow.rows_affected())
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

async fn lock_scope(tx: &mut Transaction<'_, Postgres>, scope: &str) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 728019))")
        .bind(scope)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn load_state(tx: &mut Transaction<'_, Postgres>, scope: &str) -> Result<SessionState> {
    let state: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT state FROM prompt_audit.sessions WHERE scope = $1")
            .bind(scope)
            .fetch_optional(&mut **tx)
            .await?;
    match state {
        Some(state) => Ok(serde_json::from_value(state).context("decode audit session state")?),
        None => Ok(SessionState::default()),
    }
}

async fn save_state(
    tx: &mut Transaction<'_, Postgres>,
    scope: &str,
    state: &SessionState,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO prompt_audit.sessions (scope, state) VALUES ($1, $2::json) \
         ON CONFLICT (scope) DO UPDATE SET state = EXCLUDED.state, updated_at = now()",
    )
    .bind(scope)
    .bind(serde_json::to_string(state)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn save_key(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    name: Option<&str>,
    observed_at: DateTime<Utc>,
    directory_snapshot: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO prompt_audit.key_directory (id, present, observed_at) VALUES ($1, true, $2) \
         ON CONFLICT (id) DO UPDATE SET \
         present = CASE WHEN $3 THEN true ELSE prompt_audit.key_directory.present END, \
         observed_at = greatest(prompt_audit.key_directory.observed_at, EXCLUDED.observed_at)",
    )
    .bind(id)
    .bind(observed_at)
    .bind(directory_snapshot)
    .execute(&mut **tx)
    .await?;
    if let Some(name) = name {
        sqlx::query(
            "INSERT INTO prompt_audit.key_names (key_id, name, observed_at) VALUES ($1, $2, $3) \
             ON CONFLICT (key_id, name) DO UPDATE SET \
             observed_at = greatest(prompt_audit.key_names.observed_at, EXCLUDED.observed_at)",
        )
        .bind(id)
        .bind(name)
        .bind(observed_at)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn insert_audit(
    tx: &mut Transaction<'_, Postgres>,
    scope: &str,
    observation: &RequestObservation,
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
    let inserted = sqlx::query(
        "INSERT INTO prompt_audit.audits \
         (id, identity_scope, identity_key, identity_basis, observation_status, source_position, \
          policy_version, key_id, key_name, key_name_observed_at, first_observed_at, \
          request_id, thread_id, session_id, turn_id, input_kinds, repositories, \
          input_content_id, input_text, input_search, preview, context_count, context_gap) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,date_trunc('milliseconds',$11::timestamptz), \
                 $12,$13,$14,$15,$16::json,$17::json,$18,$19,$20,$21,$22,$23) \
         ON CONFLICT (identity_scope, identity_key) WHERE identity_key IS NOT NULL DO NOTHING",
    )
    .bind(&id)
    .bind(scope)
    .bind(&draft.identity)
    .bind(&draft.identity_basis)
    .bind(&draft.observation_status)
    .bind(i64::try_from(draft.source_position)?)
    .bind(i32::try_from(draft.policy_version)?)
    .bind(&observation.key_id)
    .bind(&draft.key_name)
    .bind(draft.key_name_observed_at)
    .bind(observation.observed_at)
    .bind(&observation.request_id)
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
    .bind(draft.context_gap)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Ok(());
    }
    save_key(
        tx,
        &observation.key_id,
        draft.key_name.as_deref(),
        draft
            .key_name_observed_at
            .unwrap_or(observation.observed_at),
        false,
    )
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
