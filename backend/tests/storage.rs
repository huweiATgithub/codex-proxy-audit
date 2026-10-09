//! Run explicitly against an isolated PostgreSQL administrator connection:
//! AUDIT_TEST_DATABASE_URL=postgres://... cargo test --test storage -- --ignored
//! Each test creates its own database and restricted role; no host data is used.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use codex_proxy_audit::{
    collection::{ContentBlock, ContextMessage, RequestObservation, ResponseSnapshot, scope},
    query::QueryService,
    store::{ObservedKey, Store, decode_text, search_text},
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

struct Database {
    administrator: PgPool,
    host: PgPool,
    store: Store,
    database_name: String,
    role: String,
    audit_url: String,
}

impl Database {
    async fn create() -> Result<Self> {
        let administrator_url = std::env::var("AUDIT_TEST_DATABASE_URL")
            .context("AUDIT_TEST_DATABASE_URL must point to an isolated administrator database")?;
        let administrator = PgPool::connect(&administrator_url).await?;
        let suffix = Uuid::new_v4().simple().to_string();
        let database_name = format!("audit_storage_{suffix}");
        let role = format!("audit_role_{suffix}");
        let password = Uuid::new_v4().simple().to_string();
        // These identifiers and the password contain only our generated ASCII
        // hexadecimal characters, never caller-controlled SQL.
        sqlx::query(&format!("CREATE DATABASE {database_name}"))
            .execute(&administrator)
            .await?;
        let mut database_url = url::Url::parse(&administrator_url)?;
        database_url.set_path(&database_name);
        let host = PgPool::connect(database_url.as_str()).await?;
        sqlx::raw_sql(
            "CREATE TABLE public.model_requests (id TEXT PRIMARY KEY, client_api_key_ref TEXT, \
             user_agent TEXT, requested_model_id TEXT, reasoning_effort TEXT, secret_data TEXT); \
             CREATE VIEW public.model_request_observations AS \
             SELECT id,client_api_key_ref,user_agent,requested_model_id,reasoning_effort,secret_data \
             FROM public.model_requests; \
             CREATE TABLE public.client_api_keys (secret TEXT);",
        )
        .execute(&host)
        .await?;
        sqlx::query(&format!(
            "CREATE ROLE {role} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT PASSWORD '{password}'"
        ))
        .execute(&administrator)
        .await?;
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA prompt_audit AUTHORIZATION {role}; \
             REVOKE ALL ON SCHEMA prompt_audit FROM PUBLIC; \
             GRANT USAGE ON SCHEMA public TO {role}; \
             GRANT SELECT(id,client_api_key_ref,user_agent,requested_model_id,reasoning_effort) \
             ON public.model_request_observations TO {role};"
        ))
        .execute(&host)
        .await?;
        database_url
            .set_username(&role)
            .map_err(|()| anyhow::anyhow!("set test role"))?;
        database_url
            .set_password(Some(&password))
            .map_err(|()| anyhow::anyhow!("set test role password"))?;
        let audit_url = database_url.to_string();
        let store = Store::connect(&audit_url).await?;
        Ok(Self {
            administrator,
            host,
            store,
            database_name,
            role,
            audit_url,
        })
    }

    async fn query(&self, time_zone: &str) -> Result<QueryService> {
        QueryService::new(
            self.store.pool().clone(),
            time_zone,
            512 * 1024,
            self.store.secret("query_cursor").await?,
        )
        .map_err(anyhow::Error::msg)
    }

    async fn clean(self) -> Result<()> {
        self.store.close().await;
        self.host.close().await;
        sqlx::query(&format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.database_name
        ))
        .execute(&self.administrator)
        .await?;
        sqlx::query(&format!("DROP ROLE {}", self.role))
            .execute(&self.administrator)
            .await?;
        self.administrator.close().await;
        Ok(())
    }
}

fn observation(key: &str, thread: &str, input: Value) -> RequestObservation {
    RequestObservation {
        key_id: key.to_string(),
        request_id: Uuid::new_v4().to_string(),
        body: json!({"client_metadata":{"thread_id":thread},"input":input}),
        headers: BTreeMap::new(),
        observed_at: DateTime::parse_from_rfc3339("2026-10-09T03:59:59.123456Z")
            .unwrap()
            .with_timezone(&Utc),
        key_name: Some("原名称".to_string()),
        key_name_observed_at: Some(Utc::now()),
        incremental: false,
    }
}

fn user(id: &str, text: &str) -> Value {
    json!({"id":id,"role":"user","content":[{"type":"input_text","text":text}],
           "internal_chat_message_metadata_passthrough":{"turn_id":"same-turn","content_item_kinds":["user.input"]}})
}

fn assistant(text: &str) -> ContextMessage {
    ContextMessage {
        role: "assistant".to_string(),
        phase: Some("final_answer".to_string()),
        truncated: false,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

async fn audit_count(store: &Store) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM prompt_audit.audits")
            .fetch_one(store.pool())
            .await?,
    )
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn concurrent_identity_survives_restart_and_fact_is_independent_of_state() -> Result<()> {
    let database = Database::create().await?;
    let request = observation("key-a", "thread-a", json!([user("message-a", "请继续")]));
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let store = database.store.clone();
        let request = request.clone();
        tasks.push(tokio::spawn(async move {
            store.collect_request(&request).await
        }));
    }
    for task in tasks {
        task.await??;
    }
    assert_eq!(audit_count(&database.store).await?, 1);
    let original: Value = sqlx::query_scalar("SELECT blocks FROM prompt_audit.contents")
        .fetch_one(database.store.pool())
        .await?;
    let secret = database.store.secret("query_cursor").await?;
    let restarted = Store::connect(&database.audit_url).await?;
    assert_eq!(secret, restarted.secret("query_cursor").await?);
    let mut replay = request.clone();
    replay.request_id = Uuid::new_v4().to_string();
    replay.observed_at = Utc::now();
    restarted.collect_request(&replay).await?;
    assert_eq!(audit_count(&restarted).await?, 1);
    sqlx::query("DELETE FROM prompt_audit.sessions")
        .execute(restarted.pool())
        .await?;
    restarted.collect_request(&replay).await?;
    assert_eq!(audit_count(&restarted).await?, 1);
    assert_eq!(original[0]["text"], "请继续");
    restarted.close().await;
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn failed_body_write_rolls_back_audit_identity_and_session_together() -> Result<()> {
    let database = Database::create().await?;
    sqlx::raw_sql(
        "CREATE FUNCTION prompt_audit.reject_body() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'test body write failure'; END $$; \
         CREATE TRIGGER reject_body BEFORE INSERT ON prompt_audit.contents \
         FOR EACH ROW EXECUTE FUNCTION prompt_audit.reject_body();",
    )
    .execute(database.store.pool())
    .await?;
    let request = observation("key-a", "thread-a", json!([user("input-a", "原始正文")]));
    assert!(database.store.collect_request(&request).await.is_err());
    assert_eq!(audit_count(&database.store).await?, 0);
    let state_count: i64 = sqlx::query_scalar("SELECT count(*) FROM prompt_audit.sessions")
        .fetch_one(database.store.pool())
        .await?;
    assert_eq!(state_count, 0);
    sqlx::query("DROP TRIGGER reject_body ON prompt_audit.contents")
        .execute(database.store.pool())
        .await?;
    database.store.collect_request(&request).await?;
    assert_eq!(audit_count(&database.store).await?, 1);
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn blocked_scope_times_out_without_facts_and_recovers_after_lock_release() -> Result<()> {
    let database = Database::create().await?;
    let request = observation("key-a", "thread-a", json!([user("input-a", "锁后恢复")]));
    // Use an independent administrator connection to simulate another plugin
    // instance holding this session lock indefinitely.
    let mut holder = database.host.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 728019))")
        .bind(scope(&request))
        .execute(&mut *holder)
        .await?;
    let started = Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        database.store.collect_request(&request),
    )
    .await
    .context("scope contention must not block collection beyond three seconds")?
    .expect_err("the held session lock must abort this collection attempt");
    assert!(started.elapsed() < Duration::from_secs(3));
    match error.downcast_ref::<sqlx::Error>() {
        Some(sqlx::Error::Database(error)) => {
            assert_eq!(error.code().as_deref(), Some("55P03"));
        }
        other => panic!("expected PostgreSQL lock timeout, got {other:?}"),
    }
    assert_eq!(audit_count(&database.store).await?, 0);
    let states: i64 = sqlx::query_scalar("SELECT count(*) FROM prompt_audit.sessions")
        .fetch_one(database.store.pool())
        .await?;
    assert_eq!(states, 0);
    holder.rollback().await?;
    tokio::time::timeout(
        Duration::from_secs(3),
        database.store.collect_request(&request),
    )
    .await
    .context("a released scope must accept the next collection attempt")??;
    assert_eq!(audit_count(&database.store).await?, 1);
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn history_and_same_turn_inputs_keep_separate_immutable_contexts() -> Result<()> {
    let database = Database::create().await?;
    let request = observation(
        "key-a",
        "thread-a",
        json!([user("first", "请继续"),{"role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"最初方案"}]},user("second", "请继续")]),
    );
    let prepared = database.store.collect_request(&request).await?;
    assert_eq!(prepared.audits.len(), 2);
    assert_eq!(prepared.audits[0].context.len(), 0);
    assert_eq!(prepared.audits[1].context.len(), 2);
    database
        .store
        .persist_response(
            &prepared.response_context.unwrap(),
            &ResponseSnapshot {
                response_id: "response-a".to_string(),
                messages: vec![assistant("后续输出")],
                complete: true,
            },
        )
        .await?;
    let mut appended = request.clone();
    appended.request_id = Uuid::new_v4().to_string();
    appended.body["input"]
        .as_array_mut()
        .unwrap()
        .push(user("third", "请继续"));
    database.store.collect_request(&appended).await?;
    assert_eq!(audit_count(&database.store).await?, 3);
    let rows = sqlx::query(
        "SELECT source_position, context_count FROM prompt_audit.audits ORDER BY source_position",
    )
    .fetch_all(database.store.pool())
    .await?;
    assert_eq!(rows[0].try_get::<i64, _>("context_count")?, 0);
    assert_eq!(rows[1].try_get::<i64, _>("context_count")?, 2);
    assert_eq!(rows[2].try_get::<i64, _>("context_count")?, 3);
    let bodies: Vec<Value> =
        sqlx::query_scalar("SELECT blocks FROM prompt_audit.contents WHERE ordinal >= 0")
            .fetch_all(database.store.pool())
            .await?;
    assert!(
        bodies
            .iter()
            .all(|body| !body.to_string().contains("后续输出"))
    );
    let turns: Vec<String> = sqlx::query_scalar("SELECT turn_id FROM prompt_audit.audits")
        .fetch_all(database.store.pool())
        .await?;
    assert!(turns.iter().all(|turn| turn == "same-turn"));
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn scoped_predecessor_and_key_directory_never_cross_key_or_thread() -> Result<()> {
    let database = Database::create().await?;
    let first = database
        .store
        .collect_request(&observation(
            "key-a",
            "thread-a",
            json!([user("same-id", "用户A")]),
        ))
        .await?;
    database
        .store
        .persist_response(
            &first.response_context.unwrap(),
            &ResponseSnapshot {
                response_id: "response-a".to_string(),
                messages: vec![assistant("私有上文")],
                complete: true,
            },
        )
        .await?;
    for (key, thread) in [("key-b", "thread-a"), ("key-a", "thread-b")] {
        let mut request = observation(key, thread, json!([user("same-id", "请继续")]));
        request.incremental = true;
        request.body["previous_response_id"] = json!("response-a");
        let collected = database.store.collect_request(&request).await?;
        assert_eq!(collected.audits.len(), 1);
        assert!(collected.audits[0].context.is_empty());
        assert!(collected.audits[0].context_gap);
    }
    assert_eq!(audit_count(&database.store).await?, 3);
    database
        .store
        .update_keys(
            &[ObservedKey {
                id: "key-a".to_string(),
                name: Some("新名称".to_string()),
            }],
            Utc::now(),
        )
        .await?;
    database.store.update_keys(&[], Utc::now()).await?;
    let service = database.query("UTC").await?;
    let result = service.handle("api/keys", "").await;
    assert_eq!(result.status, 200);
    let key = result.value["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "key-a")
        .unwrap();
    assert_eq!(key["historical"], true);
    assert!(key["names"].as_array().unwrap().contains(&json!("原名称")));
    assert!(key["names"].as_array().unwrap().contains(&json!("新名称")));
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn host_metadata_missing_is_null_but_query_failure_is_an_error() -> Result<()> {
    let database = Database::create().await?;
    let request = observation(
        "key-a",
        "thread-a",
        json!([user("input-a", "正文保持可读")]),
    );
    database.store.collect_request(&request).await?;
    assert_eq!(
        database
            .store
            .request_metadata(Some(&request.request_id), "key-a")
            .await?,
        None
    );
    sqlx::query("INSERT INTO public.model_requests VALUES ($1,'key-a','test-UA','requested-model','high','hidden')")
        .bind(&request.request_id).execute(&database.host).await?;
    let metadata = database
        .store
        .request_metadata(Some(&request.request_id), "key-a")
        .await?
        .unwrap();
    assert_eq!(metadata.model.as_deref(), Some("requested-model"));
    assert!(
        database
            .store
            .request_metadata(Some(&request.request_id), "key-b")
            .await?
            .is_none()
    );
    assert!(
        sqlx::query("SELECT secret_data FROM public.model_request_observations")
            .fetch_all(database.store.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("SELECT id FROM public.model_requests")
            .fetch_all(database.store.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("SELECT secret FROM public.client_api_keys")
            .fetch_all(database.store.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM public.model_requests")
            .execute(database.store.pool())
            .await
            .is_err()
    );
    sqlx::query("DELETE FROM public.model_requests")
        .execute(&database.host)
        .await?;
    assert!(
        database
            .store
            .request_metadata(Some(&request.request_id), "key-a")
            .await?
            .is_none()
    );
    assert_eq!(audit_count(&database.store).await?, 1);
    let id: String = sqlx::query_scalar("SELECT id FROM prompt_audit.audits")
        .fetch_one(database.store.pool())
        .await?;
    let service = database.query("UTC").await?;
    assert_eq!(
        service.handle("api/audit", &format!("id={id}")).await.value["requestMetadata"],
        Value::Null
    );
    sqlx::query(&format!(
        "REVOKE SELECT(user_agent) ON public.model_request_observations FROM {}",
        database.role
    ))
    .execute(&database.host)
    .await?;
    assert!(
        database
            .store
            .request_metadata(Some(&request.request_id), "key-a")
            .await
            .is_err()
    );
    let failure = service.handle("api/audit", &format!("id={id}")).await;
    assert_eq!(failure.status, 500);
    assert_eq!(failure.value["error"]["code"], "internal_error");
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn nul_unicode_literal_search_segmentation_and_timezone_boundaries() -> Result<()> {
    let database = Database::create().await?;
    let text = format!("{}\0AbC%_\\éÉ尾部", "🦀".repeat(1500));
    let request = observation("key-a", "thread-a", json!([user("unicode-a", &text)]));
    database.store.collect_request(&request).await?;
    let projection: String = sqlx::query_scalar("SELECT input_text FROM prompt_audit.audits")
        .fetch_one(database.store.pool())
        .await?;
    assert_eq!(decode_text(&projection)?, text);
    assert!(search_text("É").find(&search_text("é")).is_none());
    let service = database.query("America/New_York").await?;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", "\0aBc%_\\")
        .append_pair("from", "2026-10-08")
        .append_pair("to", "2026-10-08")
        .finish();
    let listed = service.handle("api/audits", &query).await;
    assert_eq!(listed.status, 200, "{}", listed.value);
    assert_eq!(listed.value["total"], 1);
    assert!(
        listed.value["items"][0]["preview"]
            .as_str()
            .unwrap()
            .contains("\0AbC%_\\")
    );
    assert_eq!(
        listed.value["items"][0]["recordedAt"],
        "2026-10-09T03:59:59.123Z"
    );
    assert_eq!(
        listed.value["items"][0]["recordedAtDisplay"],
        "2026-10-08 23:59:59"
    );
    assert_eq!(
        service
            .handle("api/audits", "from=2026-10-09&to=2026-10-09")
            .await
            .value["total"],
        0
    );
    let id = listed.value["items"][0]["id"].as_str().unwrap();
    let detail = service.handle("api/audit", &format!("id={id}")).await;
    assert_eq!(detail.status, 200);
    assert_eq!(
        detail.value["input"]["characterCount"],
        text.chars().count()
    );
    let content_id = detail.value["input"]["id"].as_str().unwrap();
    let first = service
        .handle(
            "api/audit-content",
            &format!("id={id}&contentId={content_id}"),
        )
        .await;
    assert_eq!(first.status, 200);
    let segments = first.value["blocks"].as_array().unwrap();
    assert_eq!(segments[0]["text"].as_str().unwrap().chars().count(), 1200);
    assert_eq!(segments[0]["offset"], 0);
    let mut rebuilt = segments
        .iter()
        .map(|block| block["text"].as_str().unwrap())
        .collect::<String>();
    let cursor = first.value["nextCursor"].as_str().unwrap();
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("id", id)
        .append_pair("contentId", content_id)
        .append_pair("cursor", cursor)
        .finish();
    let second = service.handle("api/audit-content", &query).await;
    assert_eq!(second.status, 200);
    assert_eq!(second.value["blocks"][0]["offset"], 1200);
    for block in second.value["blocks"].as_array().unwrap() {
        rebuilt.push_str(block["text"].as_str().unwrap());
    }
    assert_eq!(rebuilt, text);
    assert_eq!(second.value["nextCursor"], Value::Null);
    database.clean().await
}

#[test]
fn codepoint_projection_keeps_literal_nul_and_ascii_only_folding() {
    let text = "éÉaA🦀\0x000061";
    assert_eq!(decode_text(&search_text(text)).unwrap(), "éÉaa🦀\0x000061");
    assert!(!search_text("É").contains(&search_text("é")));
    assert!(!search_text("a").contains(&search_text("\0")));
    assert!(decode_text("x00").is_err());
    assert!(decode_text("x00d800").is_err());
}
