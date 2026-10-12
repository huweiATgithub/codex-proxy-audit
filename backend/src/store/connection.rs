//! Keep execution deadlines and cancellation inside the SQLite connection lease.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use sqlx::{Sqlite, SqlitePool, pool::PoolConnection};

use super::StoreError;

pub(super) const STATEMENT_WAIT: Duration = Duration::from_millis(1_500);
pub(super) const CLEANUP_WAIT: Duration = Duration::from_secs(3);

pub(super) struct StatementBudget {
    origin: Instant,
    deadline: AtomicU64,
    cancelled: AtomicBool,
    wait: Duration,
}

impl StatementBudget {
    pub(super) fn start(&self) {
        let deadline = self.origin.elapsed().saturating_add(self.wait).as_nanos();
        self.deadline.store(
            u64::try_from(deadline).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    fn keep_running(&self) -> bool {
        !self.cancelled.load(Ordering::Relaxed)
            && self.origin.elapsed().as_nanos() < u128::from(self.deadline.load(Ordering::Relaxed))
    }
}

pub(super) struct ConnectionLease {
    pub(super) connection: PoolConnection<Sqlite>,
    pub(super) budget: Arc<StatementBudget>,
}

impl ConnectionLease {
    pub(super) async fn acquire(pool: &SqlitePool, wait: Duration) -> Result<Self, StoreError> {
        let mut connection = pool.acquire().await?;
        let budget = Arc::new(StatementBudget {
            origin: Instant::now(),
            deadline: AtomicU64::new(u64::MAX),
            cancelled: AtomicBool::new(false),
            wait,
        });
        let progress = Arc::clone(&budget);
        connection
            .lock_handle()
            .await?
            .set_progress_handler(1_000, move || progress.keep_running());
        Ok(Self { connection, budget })
    }
}

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        // Dropping an async SQLx future does not stop the driver's worker. The
        // progress callback stops outstanding work before the pool reuses it.
        self.budget.cancelled.store(true, Ordering::Relaxed);
    }
}
