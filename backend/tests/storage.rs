//! 验证审计批量写入、数据库升级和受限角色查询
//! 每项测试创建隔离数据库，使用 AUDIT_TEST_DATABASE_URL 指定管理员连接

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use codex_proxy_audit::{
    collection::{AuditDraft, RequestObservation, collect},
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
        Self::create_at_version(0).await
    }

    async fn create_at_version(version: i64) -> Result<Self> {
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
        if version == 1 {
            let legacy = PgPool::connect(&audit_url).await?;
            sqlx::raw_sql(include_str!("../migrations/0001_audit.sql"))
                .execute(&legacy)
                .await?;
            sqlx::raw_sql(
                "CREATE TABLE prompt_audit.schema_migrations \
                 (version BIGINT PRIMARY KEY, applied_at TIMESTAMPTZ NOT NULL DEFAULT now()); \
                 INSERT INTO prompt_audit.schema_migrations(version) VALUES (1); \
                 INSERT INTO prompt_audit.sessions(scope,state) VALUES ('old-session','{}');",
            )
            .execute(&legacy)
            .await?;
            let text = "升级前的正文\0🦀";
            sqlx::query(
                "INSERT INTO prompt_audit.audits \
                 (id,identity_scope,identity_key,identity_basis,observation_status,source_position, \
                  policy_version,key_id,key_name,first_observed_at,request_id,input_kinds, \
                  input_content_id,input_text,input_search,preview,context_count,context_gap) \
                 VALUES ('old-audit','old-scope','old-identity','message_id','identified',0,1, \
                         'key-a','原名称','2026-10-09T03:59:59.123Z','old-request','[null]', \
                         'old-content',$1,$2,$1,0,false)",
            )
            .bind(codex_proxy_audit::store::encode_text(text))
            .bind(search_text(text))
            .execute(&legacy)
            .await?;
            sqlx::query(
                "INSERT INTO prompt_audit.contents \
                 (id,audit_id,ordinal,truncated,blocks,character_count,block_count) \
                 VALUES ('old-content','old-audit',-1,false,$1::json,$2,1)",
            )
            .bind(json!([{"kind":"text","text":text}]).to_string())
            .bind(i64::try_from(text.chars().count())?)
            .execute(&legacy)
            .await?;
            legacy.close().await;
        }
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
            time_zone.parse().map_err(anyhow::Error::msg)?,
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
    }
}

fn user(id: &str, text: &str) -> Value {
    json!({"id":id,"role":"user","content":[{"type":"input_text","text":text}],
           "internal_chat_message_metadata_passthrough":{"turn_id":"same-turn","content_item_kinds":["user.input"]}})
}

fn draft(observation: &RequestObservation) -> AuditDraft {
    collect(observation).expect("fixture ends with a collectable user message")
}

async fn persist(store: &Store, observation: &RequestObservation) -> Result<()> {
    store.persist_batch(&[draft(observation)]).await
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
async fn repeated_and_concurrent_drafts_all_persist_after_restart() -> Result<()> {
    let database = Database::create().await?;
    let input = draft(&observation(
        "key-a",
        "thread-a",
        json!([user("message-a", "请继续")]),
    ));
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let store = database.store.clone();
        let input = input.clone();
        tasks.push(tokio::spawn(
            async move { store.persist_batch(&[input]).await },
        ));
    }
    for task in tasks {
        task.await??;
    }
    assert_eq!(audit_count(&database.store).await?, 12);
    let secret = database.store.secret("query_cursor").await?;
    let restarted = Store::connect(&database.audit_url).await?;
    assert_eq!(secret, restarted.secret("query_cursor").await?);
    restarted.persist_batch(&[input.clone(), input]).await?;
    assert_eq!(audit_count(&restarted).await?, 14);
    let bodies: Vec<Value> = sqlx::query_scalar("SELECT blocks FROM prompt_audit.contents")
        .fetch_all(restarted.pool())
        .await?;
    assert_eq!(bodies.len(), 14);
    assert!(bodies.iter().all(|body| body[0]["text"] == "请继续"));
    restarted.close().await;
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn failed_body_write_rolls_back_entire_batch_and_contents() -> Result<()> {
    let database = Database::create().await?;
    sqlx::raw_sql(
        "CREATE FUNCTION prompt_audit.reject_body() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN IF NEW.blocks::jsonb @> '[{\"kind\":\"text\",\"text\":\"拒绝正文\"}]'::jsonb \
         THEN RAISE EXCEPTION 'test body write failure'; END IF; RETURN NEW; END $$; \
         CREATE TRIGGER reject_body BEFORE INSERT ON prompt_audit.contents \
         FOR EACH ROW EXECUTE FUNCTION prompt_audit.reject_body();",
    )
    .execute(database.store.pool())
    .await?;
    let drafts = [
        draft(&observation(
            "key-a",
            "thread-a",
            json!([user("input-a", "原始正文")]),
        )),
        draft(&observation(
            "key-b",
            "thread-b",
            json!([user("input-b", "拒绝正文")]),
        )),
    ];
    assert!(database.store.persist_batch(&drafts).await.is_err());
    assert_eq!(audit_count(&database.store).await?, 0);
    for table in ["contents", "key_directory", "key_names"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM prompt_audit.{table}"))
            .fetch_one(database.store.pool())
            .await?;
        assert_eq!(count, 0, "partial batch remained in {table}");
    }
    sqlx::query("DROP TRIGGER reject_body ON prompt_audit.contents")
        .execute(database.store.pool())
        .await?;
    database.store.persist_batch(&drafts).await?;
    assert_eq!(audit_count(&database.store).await?, 2);
    let contents: i64 = sqlx::query_scalar("SELECT count(*) FROM prompt_audit.contents")
        .fetch_one(database.store.pool())
        .await?;
    assert_eq!(contents, 2);
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn blocked_database_write_times_out_and_recovers_after_lock_release() -> Result<()> {
    let database = Database::create().await?;
    let input = draft(&observation(
        "key-a",
        "thread-a",
        json!([user("input-a", "锁后恢复")]),
    ));
    let mut holder = database.host.begin().await?;
    sqlx::query("LOCK TABLE prompt_audit.audits IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await?;
    let started = Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        database.store.persist_batch(std::slice::from_ref(&input)),
    )
    .await
    .context("database contention must remain bounded")?
    .expect_err("the held table lock must abort this batch");
    assert!(started.elapsed() < Duration::from_secs(3));
    match error.downcast_ref::<sqlx::Error>() {
        Some(sqlx::Error::Database(error)) => {
            assert_eq!(error.code().as_deref(), Some("55P03"));
        }
        other => panic!("expected PostgreSQL lock timeout, got {other:?}"),
    }
    holder.rollback().await?;
    assert_eq!(audit_count(&database.store).await?, 0);
    database.store.persist_batch(&[input]).await?;
    assert_eq!(audit_count(&database.store).await?, 1);
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn latest_input_keeps_immutable_context_from_its_own_request() -> Result<()> {
    let database = Database::create().await?;
    let request = observation(
        "key-a",
        "thread-a",
        json!([user("first", "最初问题"),{"role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"最初方案"}]},user("second", "请继续")]),
    );
    let input = draft(&request);
    assert_eq!(input.context.len(), 2);
    database.store.persist_batch(&[input]).await?;
    let initial: Vec<Value> =
        sqlx::query_scalar("SELECT blocks FROM prompt_audit.contents ORDER BY ordinal")
            .fetch_all(database.store.pool())
            .await?;
    let mut appended = request.clone();
    appended.request_id = Uuid::new_v4().to_string();
    appended.body["input"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":"后续输出"}),
        user("third", "再继续"),
    ]);
    persist(&database.store, &appended).await?;
    assert_eq!(audit_count(&database.store).await?, 2);
    let contexts: Vec<i64> =
        sqlx::query_scalar("SELECT context_count FROM prompt_audit.audits ORDER BY context_count")
            .fetch_all(database.store.pool())
            .await?;
    assert_eq!(contexts, [2, 4]);
    let original: Vec<Value> = sqlx::query_scalar(
        "SELECT c.blocks FROM prompt_audit.contents c JOIN prompt_audit.audits a ON a.id=c.audit_id \
         WHERE a.request_id=$1 ORDER BY c.ordinal",
    )
    .bind(&request.request_id)
    .fetch_all(database.store.pool())
    .await?;
    assert_eq!(original, initial);
    assert_eq!(original[0][0]["text"], "请继续");
    assert_eq!(original[1][0]["text"], "最初问题");
    assert_eq!(original[2][0]["text"], "最初方案");
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn key_directory_keeps_observed_names_when_key_disappears() -> Result<()> {
    let database = Database::create().await?;
    persist(
        &database.store,
        &observation("key-a", "thread-a", json!([user("input-a", "用户A")])),
    )
    .await?;
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
async fn version_one_upgrade_preserves_audits_and_removes_deduplication_state() -> Result<()> {
    let database = Database::create_at_version(1).await?;
    assert_eq!(audit_count(&database.store).await?, 1);
    let row = sqlx::query("SELECT input_text,request_id,input_content_id FROM prompt_audit.audits WHERE id='old-audit'")
        .fetch_one(database.store.pool())
        .await?;
    assert_eq!(
        decode_text(&row.try_get::<String, _>("input_text")?)?,
        "升级前的正文\0🦀"
    );
    assert_eq!(row.try_get::<String, _>("request_id")?, "old-request");
    assert_eq!(row.try_get::<String, _>("input_content_id")?, "old-content");
    let blocks: Value =
        sqlx::query_scalar("SELECT blocks FROM prompt_audit.contents WHERE id='old-content'")
            .fetch_one(database.store.pool())
            .await?;
    assert_eq!(blocks, json!([{"kind":"text","text":"升级前的正文\0🦀"}]));
    let version: i64 =
        sqlx::query_scalar("SELECT max(version) FROM prompt_audit.schema_migrations")
            .fetch_one(database.store.pool())
            .await?;
    assert_eq!(version, 2);
    let obsolete: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns WHERE table_schema='prompt_audit' \
         AND table_name='audits' AND column_name IN \
         ('identity_scope','identity_key','identity_basis','observation_status','source_position','policy_version','context_gap')",
    )
    .fetch_one(database.store.pool())
    .await?;
    assert_eq!(obsolete, 0);
    for object in ["prompt_audit.sessions", "prompt_audit.audit_identity"] {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(object)
            .fetch_one(database.store.pool())
            .await?;
        assert!(!exists, "obsolete object {object} remained after upgrade");
    }
    let service = database.query("UTC").await?;
    let detail = service.handle("api/audit", "id=old-audit").await;
    assert_eq!(detail.status, 200, "{}", detail.value);
    assert_eq!(detail.value["input"]["id"], "old-content");
    let input = draft(&observation(
        "key-a",
        "thread-a",
        json!([user("same-message", "新正文")]),
    ));
    database
        .store
        .persist_batch(&[input.clone(), input])
        .await?;
    assert_eq!(audit_count(&database.store).await?, 3);
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn newer_database_schema_is_rejected_without_modification() -> Result<()> {
    let database = Database::create().await?;
    sqlx::query("INSERT INTO prompt_audit.schema_migrations(version) VALUES (3)")
        .execute(database.store.pool())
        .await?;
    let error = match Store::connect(&database.audit_url).await {
        Ok(store) => {
            store.close().await;
            panic!("a newer schema must be rejected");
        }
        Err(error) => error,
    };
    assert!(error.to_string().contains("newer than this plugin"));
    assert_eq!(audit_count(&database.store).await?, 0);
    let version: i64 =
        sqlx::query_scalar("SELECT max(version) FROM prompt_audit.schema_migrations")
            .fetch_one(database.store.pool())
            .await?;
    assert_eq!(version, 3);
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
    persist(&database.store, &request).await?;
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
    persist(&database.store, &request).await?;
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

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn shared_timezone_is_parsed_persistent_and_preserves_audit_data() -> Result<()> {
    let database = Database::create().await?;
    assert_eq!(database.store.time_zone().await?, chrono_tz::UTC);
    let secret = database.store.secret("query_cursor").await?;
    let before =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM prompt_audit.schema_migrations")
            .fetch_one(database.store.pool())
            .await?;
    let save = |body: &'static [u8]| {
        QueryService::management(
            &database.store,
            secret.clone(),
            "PUT",
            "api/time-zone",
            "",
            body,
        )
    };
    assert_eq!(save(br#"{"timeZone":"Asia/Kathmandu"}"#).await.status, 200);
    for body in [
        br#"{"timeZone":"Not/AZone"}"#.as_slice(),
        br#"{"timeZone":"UTC","extra":true}"#.as_slice(),
        br#"{"timeZone":null}"#.as_slice(),
        br#"{"timeZone":"UTC","timeZone":"Asia/Tokyo"}"#.as_slice(),
    ] {
        let result = save(body).await;
        assert_eq!(result.status, 400);
        assert_eq!(result.value["error"]["code"], "invalid_query");
        assert_eq!(
            database.store.time_zone().await?,
            chrono_tz::Asia::Kathmandu
        );
    }
    let restarted = Store::connect(&database.audit_url).await?;
    assert_eq!(restarted.time_zone().await?, chrono_tz::Asia::Kathmandu);
    assert_eq!(restarted.secret("query_cursor").await?, secret);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM prompt_audit.schema_migrations")
            .fetch_one(restarted.pool())
            .await?,
        before
    );
    sqlx::query("UPDATE prompt_audit.settings SET value = $1 WHERE name = 'timeZone'")
        .bind(b"Invalid/StoredZone".as_slice())
        .execute(database.store.pool())
        .await?;
    assert!(restarted.time_zone().await.is_err());
    let failed = QueryService::management(
        &restarted,
        secret.clone(),
        "GET",
        "api/view-context",
        "",
        &[],
    )
    .await;
    assert_eq!(failed.status, 500);
    assert_eq!(save(br#"{"timeZone":"UTC"}"#).await.status, 200);
    assert_eq!(restarted.time_zone().await?, chrono_tz::UTC);
    restarted.close().await;
    database.clean().await
}

#[tokio::test]
#[ignore = "requires AUDIT_TEST_DATABASE_URL pointing to isolated PostgreSQL"]
async fn escaped_rows_and_large_key_directories_paginate_under_fixed_response_cap() -> Result<()> {
    let database = Database::create().await?;
    let drafts = (0..200)
        .map(|index| {
            let mut value = draft(&observation(
                "key-a",
                "thread-a",
                json!([user(&format!("input-{index}"), &"\u{0001}".repeat(400))]),
            ));
            value.key_name = Some("\u{0001}".repeat(512));
            value
        })
        .collect::<Vec<_>>();
    database.store.persist_batch(&drafts).await?;
    sqlx::raw_sql(
        "INSERT INTO prompt_audit.key_directory(id,present,observed_at) \
         SELECT 'large-key-' || i,false,now() FROM generate_series(1,30) i; \
         INSERT INTO prompt_audit.key_names(key_id,name,observed_at) \
         SELECT 'large-key-' || i,j::text || repeat(chr(1),500),now() \
         FROM generate_series(1,30) i CROSS JOIN generate_series(1,32) j;",
    )
    .execute(database.store.pool())
    .await?;
    let service = database.query("UTC").await?;
    for (path, query, total) in [("api/audits", "limit=200", 200), ("api/keys", "", 31)] {
        let mut cursor = None::<String>;
        let mut ids = std::collections::HashSet::new();
        let mut pages = 0;
        loop {
            let params = cursor
                .as_ref()
                .map(|cursor| format!("{query}&cursor={cursor}"))
                .unwrap_or_else(|| query.to_string());
            let response = service.handle(path, &params).await;
            assert_eq!(response.status, 200, "{}", response.value);
            assert!(serde_json::to_vec(&response.value)?.len() <= 512 * 1024);
            for item in response.value["items"].as_array().unwrap() {
                assert!(ids.insert(item["id"].as_str().unwrap().to_string()));
            }
            pages += 1;
            cursor = response.value["nextCursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        assert!(pages > 1);
        assert_eq!(ids.len(), total);
    }
    database.clean().await
}
