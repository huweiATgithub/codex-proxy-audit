//! Exercise shared settings through the same management interface as the editor.
use std::time::Duration;

use anyhow::Result;
use codex_proxy_audit::{exclusion_settings::Exclusions, store::Store};
use serde_json::{Value, json};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

struct Database {
    store: Store,
    pool: SqlitePool,
    path: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

impl Database {
    async fn create() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("audit.sqlite3");
        let store = Store::open(&path).await?;
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .busy_timeout(Duration::from_secs(1)),
            )
            .await?;
        Ok(Self {
            store,
            pool,
            path,
            _directory: directory,
        })
    }

    async fn stored(&self) -> Result<Value> {
        let value: Vec<u8> =
            sqlx::query_scalar("SELECT value FROM settings WHERE name = 'exclusions'")
                .fetch_one(&self.pool)
                .await?;
        Ok(serde_json::from_slice(&value)?)
    }
}

async fn save(exclusions: &Exclusions, value: &Value) -> codex_proxy_audit::query::QueryResponse {
    exclusions
        .management("PUT", "", &serde_json::to_vec(value).unwrap())
        .await
}

#[tokio::test]
async fn default_save_restart_and_clear_use_plugin_private_settings() -> Result<()> {
    let database = Database::create().await?;
    let exclusions = Exclusions::load(database.store.clone()).await;
    assert_eq!(
        exclusions.management("GET", "", &[]).await.value,
        json!({"userAgentPatterns":[]})
    );
    let value = json!({"userAgentPatterns":["^Internal/", "(?i) audit-client "]});
    let saved = save(&exclusions, &value).await;
    assert_eq!(saved.status, 200, "{}", saved.value);
    assert_eq!(saved.value, value);
    assert_eq!(database.stored().await?, value);
    assert_eq!(exclusions.management("GET", "", &[]).await.value, value);

    database.store.close().await;
    let reopened = Store::open(&database.path).await?;
    let restarted = Exclusions::load(reopened.clone()).await;
    assert_eq!(restarted.management("GET", "", &[]).await.value, value);
    let empty = json!({"userAgentPatterns":[]});
    assert_eq!(save(&restarted, &empty).await.value, empty);
    assert_eq!(database.stored().await?, empty);
    // Exclusion settings are separate from the host and other plugin settings.
    assert_eq!(reopened.time_zone().await?, chrono_tz::UTC);
    reopened.close().await;
    database.pool.close().await;
    Ok(())
}

#[tokio::test]
async fn rejected_requests_preserve_active_and_durable_settings() -> Result<()> {
    let database = Database::create().await?;
    let exclusions = Exclusions::load(database.store.clone()).await;
    let accepted = json!({"userAgentPatterns":["^keep/"]});
    assert_eq!(save(&exclusions, &accepted).await.status, 200);
    for body in [
        br#"{}"#.as_slice(),
        br#"{"userAgentPatterns":null}"#,
        br#"{"userAgentPatterns":[],"unexpected":true}"#,
        br#"{"userAgentPatterns":[],"userAgentPatterns":[]}"#,
        br#"{"userAgentPatterns":[42]}"#,
        br#"{"userAgentPatterns":[" "]}"#,
    ] {
        let response = exclusions.management("PUT", "", body).await;
        assert_eq!(response.status, 400, "{}", response.value);
        assert_eq!(response.value["error"]["code"], "invalid_exclusions");
        assert_eq!(exclusions.management("GET", "", &[]).await.value, accepted);
        assert_eq!(database.stored().await?, accepted);
    }
    let invalid = save(
        &exclusions,
        &json!({"userAgentPatterns":["valid", "(?=unsupported)"]}),
    )
    .await;
    assert_eq!(invalid.status, 400);
    assert_eq!(invalid.value["error"]["patternIndex"], 1);
    assert!(
        invalid.value["error"]["message"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= 300
    );
    for (method, query, body) in [
        ("GET", "", b"{}".as_slice()),
        ("GET", "extra=value", b"".as_slice()),
        ("PUT", "extra=value", b"{}".as_slice()),
        ("POST", "", b"{}".as_slice()),
    ] {
        assert_eq!(exclusions.management(method, query, body).await.status, 400);
    }
    assert_eq!(
        exclusions
            .management("PUT", "", &vec![b' '; 256 * 1024 + 1])
            .await
            .status,
        400
    );
    assert_eq!(database.stored().await?, accepted);
    database.store.close().await;
    database.pool.close().await;
    Ok(())
}

#[tokio::test]
async fn supported_lists_can_exceed_the_audit_query_body_limit() -> Result<()> {
    let database = Database::create().await?;
    let exclusions = Exclusions::load(database.store.clone()).await;
    let value = json!({"userAgentPatterns":vec!["x".repeat(400); 32]});
    assert!(serde_json::to_vec(&value)?.len() > 8192);
    let saved = save(&exclusions, &value).await;
    assert_eq!(saved.status, 200, "{}", saved.value);
    assert_eq!(database.stored().await?, value);
    database.store.close().await;
    database.pool.close().await;
    Ok(())
}

#[tokio::test]
async fn malformed_saved_settings_remain_repairable_without_defaulting_to_capture() -> Result<()> {
    let database = Database::create().await?;
    for body in [
        b"malformed JSON".as_slice(),
        br#"{"userAgentPatterns":["("]}"#,
    ] {
        sqlx::query("INSERT INTO settings(name,value) VALUES('exclusions',?1) ON CONFLICT(name) DO UPDATE SET value=excluded.value")
            .bind(body)
            .execute(&database.pool)
            .await?;
        let exclusions = Exclusions::load(database.store.clone()).await;
        assert!(exclusions.snapshot().settings().is_none());
        assert_eq!(exclusions.management("GET", "", &[]).await.status, 503);
        let repair = json!({"userAgentPatterns":[]});
        assert_eq!(save(&exclusions, &repair).await.status, 200);
        assert_eq!(exclusions.management("GET", "", &[]).await.value, repair);
        assert_eq!(database.stored().await?, repair);
    }
    database.store.close().await;
    database.pool.close().await;
    Ok(())
}

#[tokio::test]
async fn persistence_failure_keeps_the_previous_policy_and_restart_value() -> Result<()> {
    let database = Database::create().await?;
    let exclusions = Exclusions::load(database.store.clone()).await;
    let accepted = json!({"userAgentPatterns":["^saved/"]});
    assert_eq!(save(&exclusions, &accepted).await.status, 200);
    database.store.close().await;
    assert_eq!(
        save(&exclusions, &json!({"userAgentPatterns":["^failed/"]}))
            .await
            .status,
        503
    );
    assert_eq!(exclusions.management("GET", "", &[]).await.value, accepted);
    assert_eq!(database.stored().await?, accepted);
    let reopened = Store::open(&database.path).await?;
    assert_eq!(
        Exclusions::load(reopened.clone())
            .await
            .management("GET", "", &[])
            .await
            .value,
        accepted
    );
    reopened.close().await;
    database.pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_successful_saves_publish_the_last_durable_value() -> Result<()> {
    let database = Database::create().await?;
    let exclusions = Exclusions::load(database.store.clone()).await;
    let mut saves = tokio::task::JoinSet::new();
    for index in 0..8 {
        let exclusions = exclusions.clone();
        saves.spawn(async move {
            let value = json!({"userAgentPatterns":[format!("^client-{index}/")]});
            save(&exclusions, &value).await
        });
    }
    while let Some(saved) = saves.join_next().await {
        let saved = saved?;
        assert_eq!(saved.status, 200, "{}", saved.value);
    }
    assert_eq!(
        exclusions.management("GET", "", &[]).await.value,
        database.stored().await?
    );
    database.store.close().await;
    database.pool.close().await;
    Ok(())
}
