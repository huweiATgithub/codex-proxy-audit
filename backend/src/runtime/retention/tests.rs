//! 验证维护清理的每日调度、预算续接与失败和取消后的恢复

use std::cell::Cell;

use anyhow::Result;
use chrono::TimeDelta;
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};

use super::*;
use crate::collection::{AuditDraft, ContentBlock};

struct Database {
    store: Store,
    pool: SqlitePool,
    _directory: tempfile::TempDir,
}

impl Database {
    async fn new(expired: usize) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("audit.sqlite3");
        let store = Store::open(&path).await?;
        let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(path)).await?;
        for batch in vec![expired_draft(); expired].chunks(32) {
            store.persist_batch(batch).await?;
        }
        Ok(Self {
            store,
            pool,
            _directory: directory,
        })
    }

    async fn count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM audits")
            .fetch_one(&self.pool)
            .await?)
    }

    async fn close(self) {
        self.store.close().await;
        self.pool.close().await;
    }
}

fn expired_draft() -> AuditDraft {
    AuditDraft {
        key_id: "key-a".into(),
        request_id: "request-a".into(),
        observed_at: Utc::now() - TimeDelta::days(4),
        thread_id: None,
        session_id: None,
        turn_id: None,
        blocks: vec![ContentBlock::Text {
            text: "expired input".into(),
        }],
        input_kinds: vec![None],
        repositories: None,
        context: Vec::new(),
    }
}

fn budget() -> Option<Duration> {
    Some(Duration::from_secs(120))
}

#[tokio::test]
async fn first_callback_catches_up_and_completed_sweeps_wait_one_day() -> Result<()> {
    let database = Database::new(RETENTION_BATCH_SIZE as usize * 2 + 1).await?;
    let retention = Retention::new(database.store.clone());
    retention.sweep(budget).await.unwrap();
    assert_eq!(database.count().await?, 0);

    // 完成后新到期的记录留到下一轮，正常的维护 tick 不启动每日清理
    database.store.persist_batch(&[expired_draft()]).await?;
    retention.sweep(budget).await.unwrap();
    assert_eq!(database.count().await?, 1);
    tokio::time::pause();
    tokio::time::advance(INTERVAL + Duration::from_secs(1)).await;
    tokio::time::resume();
    retention.sweep(budget).await.unwrap();
    assert_eq!(database.count().await?, 0);
    database.close().await;
    Ok(())
}

#[tokio::test]
async fn limited_budget_defers_work_and_continues_committed_batches_on_next_callback() -> Result<()>
{
    let database = Database::new(RETENTION_BATCH_SIZE as usize + 1).await?;
    let retention = Retention::new(database.store.clone());
    retention
        .sweep(|| Some(BATCH_BUDGET - Duration::from_millis(1)))
        .await
        .unwrap();
    assert_eq!(database.count().await?, RETENTION_BATCH_SIZE + 1);
    retention.sweep(|| None).await.unwrap();
    assert_eq!(database.count().await?, RETENTION_BATCH_SIZE + 1);

    let available = Cell::new(true);
    retention
        .sweep(|| available.replace(false).then_some(BATCH_BUDGET))
        .await
        .unwrap();
    assert_eq!(database.count().await?, 1);
    retention.sweep(budget).await.unwrap();
    assert_eq!(database.count().await?, 0);
    database.close().await;
    Ok(())
}

#[tokio::test]
async fn failed_batch_keeps_the_sweep_due_for_host_retry() -> Result<()> {
    let database = Database::new(1).await?;
    let retention = Retention::new(database.store.clone());
    let lock = database.pool.begin_with("BEGIN IMMEDIATE").await?;
    assert_eq!(retention.sweep(budget).await, Err(CleanupError::Storage));
    lock.rollback().await?;
    assert_eq!(database.count().await?, 1);
    retention.sweep(budget).await.unwrap();
    assert_eq!(database.count().await?, 0);
    database.close().await;
    Ok(())
}

#[tokio::test]
async fn cancelled_batch_keeps_the_sweep_due_and_releases_scheduling_state() -> Result<()> {
    let database = Database::new(1).await?;
    let retention = Retention::new(database.store.clone());
    let lock = database.pool.begin_with("BEGIN IMMEDIATE").await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), retention.sweep(budget))
            .await
            .is_err()
    );
    lock.rollback().await?;
    retention.sweep(budget).await.unwrap();
    assert_eq!(database.count().await?, 0);
    database.close().await;
    Ok(())
}
