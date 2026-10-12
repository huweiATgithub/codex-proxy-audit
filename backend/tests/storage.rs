//! 验证私有 SQLite 的批量写入、持久化、并发和查询契约
//! 每项测试使用独立的临时数据库文件

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use codex_proxy_audit::{
    collection::{AuditDraft, RequestObservation, collect},
    keys::CurrentKeys,
    query::QueryService,
    runtime::Retention,
    store::{RETENTION_BATCH_SIZE, Store, decode_text, search_text},
};
use serde_json::{Value, json};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use uuid::Uuid;

struct Database {
    pool: SqlitePool,
    store: Store,
    path: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

impl Database {
    async fn create() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("private").join("audit.sqlite3");
        let store = Store::open(&path).await?;
        let pool = SqlitePoolOptions::new()
            .max_connections(3)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true)
                    .busy_timeout(Duration::from_secs(1)),
            )
            .await?;
        Ok(Self {
            pool,
            store,
            path,
            _directory: directory,
        })
    }

    async fn query(&self, time_zone: &str) -> Result<QueryService> {
        QueryService::new(
            self.store.clone(),
            time_zone.parse().map_err(anyhow::Error::msg)?,
            self.store.secret("query_cursor").await?,
            CurrentKeys::default(),
        )
        .map_err(anyhow::Error::msg)
    }

    async fn count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM audits")
            .fetch_one(&self.pool)
            .await?)
    }

    async fn clean(self) -> Result<()> {
        self.store.close().await;
        self.pool.close().await;
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
    store.persist_batch(&[draft(observation)]).await?;
    Ok(())
}

#[tokio::test]
async fn first_open_creates_private_database_and_reopen_preserves_records_and_settings()
-> Result<()> {
    let database = Database::create().await?;
    assert!(database.path.is_file());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT sqlite_version()")
            .fetch_one(&database.pool)
            .await?,
        "3.54.0"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&database.pool)
            .await?,
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
            .fetch_one(&database.pool)
            .await?,
        "wal"
    );
    let secret = database.store.secret("query_cursor").await?;
    assert_eq!(secret.len(), 32);
    database
        .store
        .save_time_zone(chrono_tz::Asia::Tokyo)
        .await?;
    persist(
        &database.store,
        &observation("key-a", "thread-a", json!([user("input", "首次部署正文")])),
    )
    .await?;
    database.store.close().await;
    let reopened = Store::open(&database.path).await?;
    assert_eq!(reopened.secret("query_cursor").await?, secret);
    assert_eq!(reopened.time_zone().await?, chrono_tz::Asia::Tokyo);
    assert_eq!(database.count().await?, 1);
    let service = QueryService::new(
        reopened.clone(),
        chrono_tz::UTC,
        secret,
        CurrentKeys::default(),
    )
    .map_err(anyhow::Error::msg)?;
    let response = service.handle("api/audits", "").await;
    assert_eq!(response.status, 200, "{}", response.value);
    assert_eq!(response.value["items"][0]["preview"], "首次部署正文");
    reopened.close().await;
    database.clean().await
}

#[tokio::test]
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
    assert_eq!(database.count().await?, 12);
    let secret = database.store.secret("query_cursor").await?;
    let restarted = Store::open(&database.path).await?;
    assert_eq!(secret, restarted.secret("query_cursor").await?);
    restarted.persist_batch(&[input.clone(), input]).await?;
    assert_eq!(database.count().await?, 14);
    let bodies: Vec<Value> = sqlx::query_scalar("SELECT blocks FROM contents")
        .fetch_all(&database.pool)
        .await?;
    assert_eq!(bodies.len(), 14);
    assert!(bodies.iter().all(|body| body[0]["text"] == "请继续"));
    restarted.close().await;
    database.clean().await
}

#[tokio::test]
async fn failed_body_write_rolls_back_entire_batch_and_contents() -> Result<()> {
    let database = Database::create().await?;
    sqlx::raw_sql(
        "CREATE TRIGGER reject_body BEFORE INSERT ON contents \
         WHEN json_extract(NEW.blocks, '$[0].text') = '拒绝正文' \
         BEGIN SELECT RAISE(ABORT, 'test body write failure'); END;",
    )
    .execute(&database.pool)
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
    assert_eq!(database.count().await?, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM contents")
            .fetch_one(&database.pool)
            .await?,
        0
    );
    sqlx::query("DROP TRIGGER reject_body")
        .execute(&database.pool)
        .await?;
    database.store.persist_batch(&drafts).await?;
    assert_eq!(database.count().await?, 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM contents")
            .fetch_one(&database.pool)
            .await?,
        2
    );
    database.clean().await
}

#[tokio::test]
async fn blocked_database_write_times_out_and_recovers_after_lock_release() -> Result<()> {
    let database = Database::create().await?;
    let input = draft(&observation(
        "key-a",
        "thread-a",
        json!([user("input-a", "锁后恢复")]),
    ));
    database
        .store
        .persist_batch(std::slice::from_ref(&input))
        .await?;
    let service = database.query("UTC").await?;
    let mut holder = database.pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *holder).await?;
    sqlx::query("DELETE FROM audits")
        .execute(&mut *holder)
        .await?;
    // WAL readers continue to see committed rows while another connection writes.
    let response = service.handle("api/audits", "").await;
    assert_eq!(response.status, 200, "{}", response.value);
    assert_eq!(response.value["total"], 1);
    let started = Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        database.store.persist_batch(std::slice::from_ref(&input)),
    )
    .await
    .context("database contention must remain bounded")?
    .expect_err("the held write transaction must abort this batch");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        error.is_unavailable(),
        "write contention must report unavailable: {error}"
    );
    sqlx::query("ROLLBACK").execute(&mut *holder).await?;
    drop(holder);
    assert_eq!(database.count().await?, 1);
    database.store.persist_batch(&[input]).await?;
    assert_eq!(database.count().await?, 2);
    database.clean().await
}

#[tokio::test]
async fn interrupted_batch_rolls_back_all_rows_and_can_retry() -> Result<()> {
    let database = Database::create().await?;
    sqlx::raw_sql(
        "CREATE TRIGGER slow_body BEFORE INSERT ON contents \
        WHEN json_extract(NEW.blocks, '$[0].text') = 'slow' BEGIN \
        SELECT (WITH RECURSIVE work(x) AS (VALUES(0) UNION ALL \
            SELECT x+1 FROM work WHERE x<1000000000) SELECT sum(x) FROM work); END;",
    )
    .execute(&database.pool)
    .await?;
    let drafts = [
        draft(&observation(
            "key-a",
            "thread-a",
            json!([user("first", "completed earlier in batch")]),
        )),
        draft(&observation(
            "key-a",
            "thread-a",
            json!([user("second", "slow")]),
        )),
    ];
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        database.store.persist_batch(&drafts),
    )
    .await
    .context("ordinary SQL execution must remain bounded")?;
    assert!(result.is_err());
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(database.count().await?, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM contents")
            .fetch_one(&database.pool)
            .await?,
        0
    );
    sqlx::query("DROP TRIGGER slow_body")
        .execute(&database.pool)
        .await?;
    database.store.persist_batch(&drafts).await?;
    assert_eq!(database.count().await?, 2);
    database.clean().await
}

#[tokio::test]
async fn cancelled_batch_stops_the_driver_worker_rolls_back_and_can_retry() -> Result<()> {
    let database = Database::create().await?;
    sqlx::raw_sql(
        "CREATE TRIGGER slow_body BEFORE INSERT ON contents \
         WHEN json_extract(NEW.blocks, '$[0].text') = 'slow' BEGIN \
         SELECT (WITH RECURSIVE work(x) AS (VALUES(0) UNION ALL \
             SELECT x+1 FROM work WHERE x<1000000000) SELECT sum(x) FROM work); END;",
    )
    .execute(&database.pool)
    .await?;
    let drafts = [
        draft(&observation(
            "key-a",
            "thread-a",
            json!([user("first", "completed earlier in batch")]),
        )),
        draft(&observation(
            "key-a",
            "thread-a",
            json!([user("second", "slow")]),
        )),
    ];
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            database.store.persist_batch(&drafts),
        )
        .await
        .is_err()
    );
    // Removing the trigger needs a write lock: succeeding promptly proves the
    // abandoned worker stopped and released its transaction after cancellation.
    tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query("DROP TRIGGER slow_body").execute(&database.pool),
    )
    .await
    .context("cancelled SQL must release its write lock promptly")??;
    assert_eq!(database.count().await?, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM contents")
            .fetch_one(&database.pool)
            .await?,
        0
    );
    database.store.persist_batch(&drafts).await?;
    assert_eq!(database.count().await?, 2);
    database.clean().await
}

#[tokio::test]
async fn wal_reader_snapshot_survives_a_committed_store_write() -> Result<()> {
    let database = Database::create().await?;
    let input = draft(&observation(
        "key-a",
        "thread-a",
        json!([user("input", "snapshot")]),
    ));
    database
        .store
        .persist_batch(std::slice::from_ref(&input))
        .await?;
    let mut reader = database.pool.begin().await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audits")
            .fetch_one(&mut *reader)
            .await?,
        1
    );
    tokio::time::timeout(
        Duration::from_secs(2),
        database.store.persist_batch(&[input]),
    )
    .await
    .context("a WAL reader must not block the writer")??;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audits")
            .fetch_one(&mut *reader)
            .await?,
        1
    );
    reader.commit().await?;
    assert_eq!(database.count().await?, 2);
    database.clean().await
}

fn retention_draft(observed_at: DateTime<Utc>) -> AuditDraft {
    let mut input = draft(&observation(
        "key-retention",
        "thread-retention",
        json!([{"role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"前序方案"}]},user("input", "请继续")]),
    ));
    input.observed_at = observed_at;
    input
}

#[tokio::test]
async fn retention_preserves_the_72_hour_boundary_and_cascades_only_expired_bodies() -> Result<()> {
    let database = Database::create().await?;
    // This interval crosses New York's DST change; retention is elapsed UTC time.
    let now = DateTime::parse_from_rfc3339("2026-11-03T06:30:00.123Z")?.with_timezone(&Utc);
    let cutoff = now - chrono::TimeDelta::hours(72);
    let time_zone = "America/New_York".parse()?;
    database.store.save_time_zone(time_zone).await?;
    let secret = database.store.secret("query_cursor").await?;
    let mut inputs = Vec::new();
    for (request_id, observed_at) in [
        ("expired", cutoff - chrono::TimeDelta::milliseconds(1)),
        ("boundary", cutoff),
        ("recent", now - chrono::TimeDelta::hours(1)),
        ("future", now + chrono::TimeDelta::hours(1)),
    ] {
        let mut input = retention_draft(observed_at);
        input.request_id = request_id.to_string();
        inputs.push(input);
    }
    database.store.persist_batch(&inputs).await?;
    let expired_id: String = sqlx::query_scalar("SELECT id FROM audits WHERE request_id='expired'")
        .fetch_one(&database.pool)
        .await?;
    assert_eq!(database.store.delete_expired_batch(now).await?, 1);
    let retained: Vec<String> =
        sqlx::query_scalar("SELECT request_id FROM audits ORDER BY request_id")
            .fetch_all(&database.pool)
            .await?;
    assert_eq!(retained, ["boundary", "future", "recent"]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM contents")
            .fetch_one(&database.pool)
            .await?,
        6
    );
    assert_eq!(database.store.secret("query_cursor").await?, secret);
    assert_eq!(database.store.time_zone().await?, time_zone);
    let response = database
        .query("UTC")
        .await?
        .handle("api/audit", &format!("id={expired_id}"))
        .await;
    assert_eq!(response.status, 404);
    assert_eq!(database.store.delete_expired_batch(now).await?, 0);
    database.clean().await
}

#[tokio::test]
async fn retention_uses_the_full_cutoff_precision_against_millisecond_records() -> Result<()> {
    let database = Database::create().await?;
    let now = DateTime::parse_from_rfc3339("2026-11-03T06:30:00.123456Z")?.with_timezone(&Utc);
    let cutoff = now - chrono::TimeDelta::hours(72);
    database
        .store
        .persist_batch(&[
            retention_draft(cutoff),
            retention_draft(cutoff + chrono::TimeDelta::milliseconds(1)),
        ])
        .await?;
    let stored: Vec<i64> =
        sqlx::query_scalar("SELECT first_observed_at FROM audits ORDER BY first_observed_at")
            .fetch_all(&database.pool)
            .await?;
    assert_eq!(
        stored,
        [cutoff.timestamp_millis(), cutoff.timestamp_millis() + 1]
    );
    // The first stored time is .123000, strictly earlier than the .123456 cutoff.
    assert_eq!(database.store.delete_expired_batch(now).await?, 1);
    assert_eq!(database.count().await?, 1);
    database.clean().await
}

#[tokio::test]
async fn retention_batches_count_audits_and_allow_concurrent_cleanup_and_writes() -> Result<()> {
    let database = Database::create().await?;
    let now = Utc::now();
    let expired = retention_draft(now - chrono::TimeDelta::days(4));
    let inputs = vec![expired; (RETENTION_BATCH_SIZE * 2 + 5) as usize];
    for batch in inputs.chunks(32) {
        database.store.persist_batch(batch).await?;
    }
    let recent = [retention_draft(now)];
    let (first, second, write) = tokio::time::timeout(Duration::from_secs(4), async {
        tokio::join!(
            database.store.delete_expired_batch(now),
            database.store.delete_expired_batch(now),
            database.store.persist_batch(&recent),
        )
    })
    .await
    .context("cleanup and a concurrent writer must finish within their deadlines")?;
    assert_eq!(first?, RETENTION_BATCH_SIZE as u64);
    assert_eq!(second?, RETENTION_BATCH_SIZE as u64);
    write?;
    assert_eq!(database.count().await?, 6);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM contents")
            .fetch_one(&database.pool)
            .await?,
        12
    );
    assert_eq!(database.store.delete_expired_batch(now).await?, 5);
    assert_eq!(database.store.delete_expired_batch(now).await?, 0);
    assert_eq!(database.count().await?, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM contents")
            .fetch_one(&database.pool)
            .await?,
        2
    );
    database.clean().await
}

#[tokio::test]
async fn retention_cascade_failure_rolls_back_the_batch_and_can_retry() -> Result<()> {
    let database = Database::create().await?;
    let now = Utc::now();
    let expired = retention_draft(now - chrono::TimeDelta::days(4));
    database
        .store
        .persist_batch(&[expired.clone(), expired])
        .await?;
    sqlx::raw_sql(
        "CREATE TRIGGER reject_cascade BEFORE DELETE ON contents \
        BEGIN SELECT RAISE(ABORT, 'test cascade failure'); END;",
    )
    .execute(&database.pool)
    .await?;
    assert!(database.store.delete_expired_batch(now).await.is_err());
    assert_eq!(database.count().await?, 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM contents")
            .fetch_one(&database.pool)
            .await?,
        4
    );
    sqlx::query("DROP TRIGGER reject_cascade")
        .execute(&database.pool)
        .await?;
    assert_eq!(database.store.delete_expired_batch(now).await?, 2);
    assert_eq!(database.count().await?, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM contents")
            .fetch_one(&database.pool)
            .await?,
        0
    );
    database.clean().await
}

#[tokio::test]
async fn interrupted_cleanup_rolls_back_and_connections_recover() -> Result<()> {
    let database = Database::create().await?;
    let now = Utc::now();
    database
        .store
        .persist_batch(&[retention_draft(now - chrono::TimeDelta::days(4))])
        .await?;
    // The deliberately enormous query must be interrupted by the cleanup deadline.
    sqlx::raw_sql(
        "CREATE TRIGGER slow_delete BEFORE DELETE ON audits BEGIN \
        SELECT (WITH RECURSIVE work(x) AS (VALUES(0) UNION ALL \
            SELECT x+1 FROM work WHERE x<1000000000) SELECT sum(x) FROM work); END;",
    )
    .execute(&database.pool)
    .await?;
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        database.store.delete_expired_batch(now),
    )
    .await
    .context("cleanup execution must remain bounded")?;
    assert!(result.is_err());
    assert!(
        started.elapsed() >= Duration::from_millis(1500),
        "cleanup must retain its longer execution deadline"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(database.count().await?, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM contents")
            .fetch_one(&database.pool)
            .await?,
        2
    );
    sqlx::query("DROP TRIGGER slow_delete")
        .execute(&database.pool)
        .await?;
    assert_eq!(database.store.delete_expired_batch(now).await?, 1);
    database
        .store
        .persist_batch(&[retention_draft(now)])
        .await?;
    assert_eq!(database.count().await?, 1);
    database.clean().await
}

#[tokio::test]
async fn retention_worker_catches_up_at_startup_and_runs_daily_without_requests() -> Result<()> {
    let database = Database::create().await?;
    let now = Utc::now();
    let expired = retention_draft(now - chrono::TimeDelta::days(4));
    let inputs = vec![expired.clone(); (RETENTION_BATCH_SIZE * 2 + 1) as usize];
    for batch in inputs.chunks(32) {
        database.store.persist_batch(batch).await?;
    }
    database
        .store
        .persist_batch(&[retention_draft(now)])
        .await?;
    let worker = Retention::start(database.store.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        while database.count().await? != 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("startup must clear more than one batch without waiting for requests")??;
    database.store.persist_batch(&[expired]).await?;
    // Advance the scheduler's clock, then resume it before database I/O so
    // pool/statement timeouts still use real elapsed time.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(24 * 60 * 60 + 1)).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(5), async {
        while database.count().await? != 1 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("daily cleanup must run again without incoming requests")??;
    tokio::time::timeout(Duration::from_secs(1), worker.stop())
        .await
        .context("shutdown must not wait for the next retention interval")?;
    database.clean().await
}

#[tokio::test]
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
    let initial: Vec<Value> = sqlx::query_scalar("SELECT blocks FROM contents ORDER BY ordinal")
        .fetch_all(&database.pool)
        .await?;
    let mut appended = request.clone();
    appended.request_id = Uuid::new_v4().to_string();
    appended.body["input"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":"后续输出"}),
        user("third", "再继续"),
    ]);
    persist(&database.store, &appended).await?;
    assert_eq!(database.count().await?, 2);
    let contexts: Vec<i64> =
        sqlx::query_scalar("SELECT context_count FROM audits ORDER BY context_count")
            .fetch_all(&database.pool)
            .await?;
    assert_eq!(contexts, [2, 4]);
    let original: Vec<Value> = sqlx::query_scalar(
        "SELECT c.blocks FROM contents c JOIN audits a ON a.id=c.audit_id \
         WHERE a.request_id=$1 ORDER BY c.ordinal",
    )
    .bind(&request.request_id)
    .fetch_all(&database.pool)
    .await?;
    assert_eq!(original, initial);
    assert_eq!(original[0][0]["text"], "请继续");
    assert_eq!(original[1][0]["text"], "最初问题");
    assert_eq!(original[2][0]["text"], "最初方案");
    database.clean().await
}

#[tokio::test]
async fn current_key_names_resolve_old_rows_and_change_after_rename_or_deletion() -> Result<()> {
    let database = Database::create().await?;
    persist(
        &database.store,
        &observation("key-a", "thread-a", json!([user("input-a", "用户A")])),
    )
    .await?;
    let secret = database.store.secret("query_cursor").await?;
    let snapshot: Vec<(String, String)> = sqlx::query_as("SELECT id,key_id FROM audits")
        .fetch_all(&database.pool)
        .await?;
    for name in [Some("原名称"), Some("新名称"), None] {
        let keys = CurrentKeys::from_keys(
            name.map(|name| {
                vec![gateway_plugin_sdk::call::host::ClientKey {
                    id: "key-a".into(),
                    name: name.into(),
                    enabled: true,
                }]
            })
            .unwrap_or_default(),
        )
        .unwrap();
        let service =
            QueryService::new(database.store.clone(), chrono_tz::UTC, secret.clone(), keys)
                .unwrap();
        let list = service.handle("api/audits", "key=key-a").await;
        assert_eq!(list.status, 200);
        assert_eq!(list.value["items"][0]["keyName"], json!(name));
        let detail = service
            .handle("api/audit", &format!("id={}", snapshot[0].0))
            .await;
        assert_eq!(detail.status, 200);
        assert_eq!(detail.value["keyName"], json!(name));
        let filter = service.handle("api/keys", "").await;
        assert_eq!(
            filter.value["items"].as_array().unwrap().len(),
            usize::from(name.is_some())
        );
        if let Some(name) = name {
            assert_eq!(filter.value["items"][0]["name"], name);
        }
    }
    for path in ["api/audits", "api/audit", "api/keys"] {
        let failed = QueryService::management(
            &database.store,
            secret.clone(),
            "GET",
            path,
            "",
            &[],
            std::future::ready(Err(codex_proxy_audit::keys::KeyLookupError)),
        )
        .await;
        assert_eq!(failed.status, 503);
        assert_eq!(failed.value["error"]["code"], "unavailable");
    }
    let independent = QueryService::management(
        &database.store,
        secret.clone(),
        "GET",
        "api/view-context",
        "",
        &[],
        async { panic!("view context must not fetch names") },
    )
    .await;
    assert_eq!(independent.status, 200);
    let saved = QueryService::management(
        &database.store,
        secret.clone(),
        "PUT",
        "api/time-zone",
        "",
        br#"{"timeZone":"UTC"}"#,
        async { panic!("timezone save must not fetch names") },
    )
    .await;
    assert_eq!(saved.status, 200);
    let after: Vec<(String, String)> = sqlx::query_as("SELECT id,key_id FROM audits")
        .fetch_all(&database.pool)
        .await?;
    assert_eq!(snapshot, after);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pragma_table_info('audits') WHERE name IN ('key_name','key_name_observed_at')").fetch_one(&database.pool).await?, 0);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('key_directory','key_names')").fetch_one(&database.pool).await?, 0);
    database.clean().await
}

#[tokio::test]
async fn newer_database_schema_is_rejected_without_modification() -> Result<()> {
    let database = Database::create().await?;
    sqlx::query("PRAGMA user_version=2")
        .execute(&database.pool)
        .await?;
    let error = match Store::open(&database.path).await {
        Ok(store) => {
            store.close().await;
            panic!("a newer schema must be rejected");
        }
        Err(error) => error,
    };
    assert!(error.to_string().contains("newer than this plugin"));
    assert_eq!(database.count().await?, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&database.pool)
            .await?,
        2
    );
    database.clean().await
}

#[tokio::test]
async fn audit_detail_and_content_need_no_host_database_tables() -> Result<()> {
    let database = Database::create().await?;
    let request = observation(
        "key-a",
        "thread-a",
        json!([user("input-a", "正文保持可读")]),
    );
    persist(&database.store, &request).await?;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    ).fetch_all(&database.pool).await?;
    assert_eq!(tables, ["audits", "contents", "settings"]);
    let id: String = sqlx::query_scalar("SELECT id FROM audits")
        .fetch_one(&database.pool)
        .await?;
    let service = database.query("UTC").await?;
    let detail = service.handle("api/audit", &format!("id={id}")).await;
    assert_eq!(detail.status, 200, "{}", detail.value);
    assert!(detail.value.get("requestMetadata").is_none());
    let content_id = detail.value["input"]["id"].as_str().unwrap();
    let content = service
        .handle(
            "api/audit-content",
            &format!("id={id}&contentId={content_id}"),
        )
        .await;
    assert_eq!(content.status, 200, "{}", content.value);
    assert_eq!(content.value["blocks"][0]["text"], "正文保持可读");
    database.clean().await
}

#[tokio::test]
async fn nul_unicode_literal_search_segmentation_and_timezone_boundaries() -> Result<()> {
    let database = Database::create().await?;
    let text = format!("{}\0AbC%_\\éÉ尾部", "🦀".repeat(1500));
    let request = observation("key-a", "thread-a", json!([user("unicode-a", &text)]));
    persist(&database.store, &request).await?;
    let projection: String = sqlx::query_scalar("SELECT input_text FROM audits")
        .fetch_one(&database.pool)
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
async fn shared_timezone_is_parsed_persistent_and_preserves_audit_data() -> Result<()> {
    let database = Database::create().await?;
    assert_eq!(database.store.time_zone().await?, chrono_tz::UTC);
    let secret = database.store.secret("query_cursor").await?;
    let before = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
        .fetch_one(&database.pool)
        .await?;
    let save = |body: &'static [u8]| {
        QueryService::management(
            &database.store,
            secret.clone(),
            "PUT",
            "api/time-zone",
            "",
            body,
            std::future::ready(Ok(CurrentKeys::default())),
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
    let restarted = Store::open(&database.path).await?;
    assert_eq!(restarted.time_zone().await?, chrono_tz::Asia::Kathmandu);
    assert_eq!(restarted.secret("query_cursor").await?, secret);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&database.pool)
            .await?,
        before
    );
    sqlx::query("UPDATE settings SET value = $1 WHERE name = 'timeZone'")
        .bind(b"Invalid/StoredZone".as_slice())
        .execute(&database.pool)
        .await?;
    assert!(restarted.time_zone().await.is_err());
    let failed = QueryService::management(
        &restarted,
        secret.clone(),
        "GET",
        "api/view-context",
        "",
        &[],
        std::future::ready(Ok(CurrentKeys::default())),
    )
    .await;
    assert_eq!(failed.status, 500);
    assert_eq!(save(br#"{"timeZone":"UTC"}"#).await.status, 200);
    assert_eq!(restarted.time_zone().await?, chrono_tz::UTC);
    restarted.close().await;
    database.clean().await
}

#[tokio::test]
async fn escaped_rows_and_large_key_directories_paginate_under_fixed_response_cap() -> Result<()> {
    let database = Database::create().await?;
    let drafts = (0..200)
        .map(|index| {
            draft(&observation(
                "key-a",
                "thread-a",
                json!([user(&format!("input-{index}"), &"\u{0001}".repeat(400))]),
            ))
        })
        .collect::<Vec<_>>();
    database.store.persist_batch(&drafts).await?;
    let keys = CurrentKeys::from_keys(
        std::iter::once(gateway_plugin_sdk::call::host::ClientKey {
            id: "key-a".into(),
            name: "\u{0001}".repeat(512),
            enabled: true,
        })
        .chain((0..230).map(|i| gateway_plugin_sdk::call::host::ClientKey {
            id: format!("large-key-{i:03}"),
            name: "\u{0001}".repeat(512),
            enabled: true,
        }))
        .collect(),
    )
    .unwrap();
    let service = QueryService::new(
        database.store.clone(),
        chrono_tz::UTC,
        database.store.secret("query_cursor").await?,
        keys,
    )
    .unwrap();
    for (path, query, total) in [("api/audits", "limit=200", 200), ("api/keys", "", 231)] {
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
